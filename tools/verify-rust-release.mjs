import { createHash } from 'node:crypto'
import { createReadStream } from 'node:fs'
import { copyFile, lstat, mkdir, readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'

const [root, output, version, sourceSha, definitionSha, runId, receiptDir, migrationRunId] = process.argv.slice(2)
if (!root || !output || !receiptDir || !/^[1-9]\d*$/.test(migrationRunId || '') || !/^\d+\.\d+\.\d+$/.test(version || '') ||
    !/^[a-f0-9]{40}$/.test(sourceSha || '') || !/^[a-f0-9]{40}$/.test(definitionSha || '') ||
    !/^[1-9]\d*$/.test(runId || '')) throw new Error('invalid release verifier arguments')

const receiptNames = await readdir(receiptDir)
if (receiptNames.length !== 1 || receiptNames[0] !== 'rust-migration-receipt.json') throw new Error('invalid migration receipt artifact')
const receipt = JSON.parse(await readFile(join(receiptDir, receiptNames[0]), 'utf8'))
if (receipt.schema !== 'seuoffice-rust-migration/v2' || receipt.sourceSha !== sourceSha ||
    receipt.definitionSha !== definitionSha || receipt.workflowRunId !== migrationRunId ||
    receipt.candidateRunId !== runId || receipt.version !== version || !Array.isArray(receipt.platforms) ||
    receipt.platforms.length !== 2) throw new Error('invalid migration receipt identity')

const next = receipt.next
const parseVersion = (value) => {
  if (typeof value !== 'string' || !/^\d+\.\d+\.\d+$/.test(value)) throw new Error('invalid next version')
  return value.split('.').map(BigInt)
}
const currentParts = parseVersion(version)
const nextParts = parseVersion(next?.version)
if (!nextParts.some((part, index) => part > currentParts[index] && nextParts.slice(0, index).every((v, i) => v === currentParts[i])) ||
    !/^[1-9]\d*$/.test(next.runId || '') || !/^[a-f0-9]{40}$/.test(next.sourceSha || '') ||
    !/^[a-f0-9]{40}$/.test(next.definitionSha || '') || !Array.isArray(next.platforms) || next.platforms.length !== 2 ||
    (next.runId === runId && (next.sourceSha !== sourceSha || next.definitionSha !== definitionSha))) {
  throw new Error('invalid next candidate identity')
}
if (!Array.isArray(receipt.origins) || receipt.origins.length < 1 ||
    receipt.origins.some((origin) => typeof origin !== 'string' || !origin) ||
    new Set(receipt.origins).size !== receipt.origins.length || !Array.isArray(receipt.results) ||
    receipt.results.length !== receipt.origins.length * 5) throw new Error('invalid migration result set')
for (const origin of receipt.origins) {
  for (const [platform, fault] of [['darwin', null], ...['none', 'kill-installer', 'block-start', 'lock-file'].map((f) => ['win32', f])]) {
    const matches = receipt.results.filter((item) => item?.platform === platform && item.result?.from === origin &&
      (platform === 'darwin' || item.result.fault === fault))
    if (matches.length !== 1) throw new Error(`missing migration result ${origin}/${platform}/${fault}`)
    const result = matches[0].result
    const actualNext = next.platforms.find((item) => item?.platform === platform)
    const actualCandidate = receipt.platforms.find((item) => item?.platform === platform)
    const arch = platform === 'win32' ? 'x64' : 'arm64'
    const artifact = platform === 'win32' ? 'rust-bundle-win' : 'rust-bundle-mac'
    for (const [item, identity, stored] of [
      [result.toReceipt, { runId, version, sourceSha, definitionSha }, actualCandidate],
      [result.nextReceipt, next, actualNext],
    ]) {
      if (!item || !stored || item.platform !== platform || item.arch !== arch || item.artifact !== artifact || item.channel !== 'latest' ||
          ['runId', 'version', 'sourceSha', 'definitionSha'].some((key) => item[key] !== identity[key]) ||
          JSON.stringify(item.channels) !== JSON.stringify(stored.channels)) throw new Error(`inconsistent ${platform} migration evidence`)
    }
    if (result.to !== version || result.next !== next.version || result.passed !== true || result.skipped || result.failure) {
      throw new Error(`failed migration ${origin}/${platform}/${fault}`)
    }
    if (platform === 'darwin') {
      if (!Array.isArray(result.checks) || !['profile-initially-absent', 'seeded', 'candidate-ui-data', 'second-update-ui-data'].every((step) => result.checks.includes(step))) throw new Error(`incomplete mac migration ${origin}`)
    } else {
      if (!Array.isArray(result.steps)) throw new Error(`missing Windows steps ${origin}/${fault}`)
      const required = ['profile', 'installed', 'seeded', 'installer-started', 'after-install', 'checked']
      if (fault === 'none') required.push('installer-ended', 'second-update-download', 'second-update')
      if (fault === 'kill-installer') required.push('installer-killed', 'sign-in-recovery')
      if (fault === 'block-start') required.push('installer-ended')
      if (fault === 'lock-file') required.push('file-held', 'installer-ended')
      if (!required.every((name) => result.steps.some((step) => step?.step === name)) ||
          (fault === 'none' && !result.steps.some((step) => step?.step === 'second-update' && step.data?.version === next.version))) throw new Error(`incomplete Windows migration ${origin}/${fault}`)
    }
  }
}

const files = []
for (const [artifact, platform, arch, signing] of [
  ['rust-bundle-win', 'win32', 'x64', 'unsigned'],
  ['rust-bundle-mac', 'darwin', 'arm64', 'ad-hoc'],
]) {
  const dir = join(root, artifact)
  const manifest = JSON.parse(await readFile(join(dir, 'rust-bundle-manifest.json'), 'utf8'))
  if (manifest.schema !== 'seuoffice-rust-bundle/v1' || manifest.sourceSha !== sourceSha ||
      manifest.definitionSha !== definitionSha || manifest.workflowRunId !== runId ||
      manifest.platform !== platform || manifest.arch !== arch || !Array.isArray(manifest.versions) ||
      manifest.versions.length < 1 || manifest.versions.length > 2 ||
      manifest.versions.filter((item) => item?.version === version).length !== 1 ||
      (manifest.versions.length === 2 && (next.runId !== runId ||
        manifest.versions.filter((item) => item?.version === next.version).length !== 1)) ||
      (next.runId === runId && manifest.versions.length !== 2)) {
    throw new Error(`invalid ${artifact} manifest identity`)
  }
  const channels = manifest.versions.find((item) => item.version === version).channels
  const matchingPlatforms = receipt.platforms.filter((item) => item?.platform === platform && item.arch === arch)
  if (matchingPlatforms.length !== 1 || !Array.isArray(matchingPlatforms[0].channels) ||
      matchingPlatforms[0].channels.length !== 2) throw new Error(`invalid ${platform} migration receipt`)
  const nextPlatforms = next.platforms.filter((item) => item?.platform === platform && item.arch === arch &&
    item.artifact === artifact && item.channel === 'latest')
  if (nextPlatforms.length !== 1 || !Array.isArray(nextPlatforms[0].channels) ||
      nextPlatforms[0].channels.length !== 2 ||
      !['latest', 'stable'].every((name) => nextPlatforms[0].channels.filter((item) => item?.channel === name).length === 1)) {
    throw new Error(`invalid ${platform} next migration receipt`)
  }
  if (next.runId === runId && JSON.stringify(manifest.versions.find((item) => item.version === next.version).channels) !== JSON.stringify(nextPlatforms[0].channels)) {
    throw new Error(`next ${artifact} manifest mismatch`)
  }
  const nextNames = new Set()
  for (const channel of nextPlatforms[0].channels) {
    const suffix = channel.channel === 'stable' ? '-stable' : ''
    const names = platform === 'win32'
      ? [[`SeuOffice-Setup-${next.version}${suffix}.exe`, 'installer']]
      : [[`SeuOffice-${next.version}-arm64${suffix}.dmg`, 'installer'], [`SeuOffice-${next.version}-arm64-mac${suffix}.zip`, 'archive']]
    names.push([`${names.at(-1)[0]}.blockmap`, 'blockmap'])
    names.push([`${channel.channel}${platform === 'darwin' ? '-mac' : ''}.yml`, 'feed'])
    if (!(platform === 'win32' ? ['unsigned', 'authenticode'] : ['ad-hoc', 'developer-id-notarized']).includes(channel.signing) ||
        !Array.isArray(channel.files) || channel.files.length !== names.length) throw new Error(`invalid next ${platform} files or signing`)
    for (const [name, kind] of names) {
      const matches = channel.files.filter((item) => item?.name === name)
      if (matches.length !== 1) throw new Error(`missing or ambiguous next ${name}`)
      const entry = matches[0]
      if (entry.kind !== kind || entry.relativePath !== `${next.version}/${name}` ||
          !/^[a-f0-9]{64}$/.test(entry.sha256 || '') || !Number.isSafeInteger(entry.bytes) || entry.bytes <= 0) throw new Error(`invalid next entry ${name}`)
      if (nextNames.has(name)) throw new Error(`duplicate next entry ${name}`)
      nextNames.add(name)
      if (next.runId === runId) {
        const file = join(dir, next.version, name)
        const info = await lstat(file)
        if (!info.isFile() || info.size !== entry.bytes) throw new Error(`invalid size or type ${file}`)
        const hash = createHash('sha256')
        for await (const chunk of createReadStream(file)) hash.update(chunk)
        if (hash.digest('hex') !== entry.sha256) throw new Error(`hash mismatch ${file}`)
      }
    }
  }
  if (next.runId === runId) {
    const actualNext = await readdir(join(dir, next.version))
    if (actualNext.length !== nextNames.size || actualNext.some((name) => !nextNames.has(name))) throw new Error(`unexpected next files in ${artifact}`)
  }
  if (!Array.isArray(channels) || channels.length !== 2 ||
      !['latest', 'stable'].every((name) => channels.filter((item) => item?.channel === name).length === 1)) {
    throw new Error(`invalid ${artifact} channels`)
  }
  const expectedNames = new Set()
  for (const channel of channels) {
    const suffix = channel.channel === 'stable' ? '-stable' : ''
    const expected = platform === 'win32'
      ? [[`SeuOffice-Setup-${version}${suffix}.exe`, 'installer']]
      : [[`SeuOffice-${version}-arm64${suffix}.dmg`, 'installer'], [`SeuOffice-${version}-arm64-mac${suffix}.zip`, 'archive']]
    expected.push([`${expected.at(-1)[0]}.blockmap`, 'blockmap'])
    expected.push([`${channel.channel}${platform === 'darwin' ? '-mac' : ''}.yml`, 'feed'])
    const matchingChannels = matchingPlatforms[0].channels.filter((item) => item?.channel === channel.channel)
    if (matchingChannels.length !== 1 || matchingChannels[0].signing !== channel.signing ||
        !Array.isArray(matchingChannels[0].files) || matchingChannels[0].files.length !== expected.length) {
      throw new Error(`invalid ${platform} ${channel.channel} migration receipt`)
    }
    if (channel.signing !== signing || !Array.isArray(channel.files) || channel.files.length !== expected.length) {
      throw new Error(`invalid ${artifact} ${channel.channel} files or signing`)
    }
    for (const [name, kind] of expected) {
      const matches = channel.files.filter((entry) => entry?.name === name)
      if (matches.length !== 1) throw new Error(`missing or ambiguous ${name}`)
      const entry = matches[0]
      const receiptFiles = matchingChannels[0].files.filter((item) => item?.name === name)
      if (receiptFiles.length !== 1 || ['relativePath', 'sha256', 'bytes', 'kind'].some((key) => receiptFiles[0][key] !== entry[key])) {
        throw new Error(`migration receipt mismatch ${name}`)
      }
      if (entry.kind !== kind || entry.relativePath !== `${version}/${name}` ||
          !/^[a-f0-9]{64}$/.test(entry.sha256 || '') || !Number.isSafeInteger(entry.bytes) || entry.bytes <= 0 ||
          expectedNames.has(name)) throw new Error(`invalid entry ${name}`)
      expectedNames.add(name)
      const file = join(dir, version, name)
      const info = await lstat(file)
      if (!info.isFile() || info.size !== entry.bytes) throw new Error(`invalid size or type ${file}`)
      const hash = createHash('sha256')
      for await (const chunk of createReadStream(file)) hash.update(chunk)
      if (hash.digest('hex') !== entry.sha256) throw new Error(`hash mismatch ${file}`)
      files.push([file, name])
    }
  }
  const top = await readdir(dir)
  const actual = await readdir(join(dir, version))
  if (top.length !== manifest.versions.length + 1 || !top.includes(version) || !top.includes('rust-bundle-manifest.json') ||
      (manifest.versions.length === 2 && !top.includes(next.version)) ||
      actual.length !== expectedNames.size || actual.some((name) => !expectedNames.has(name))) {
    throw new Error(`unexpected files in ${artifact}`)
  }
}
if (new Set(files.map(([, name]) => name)).size !== files.length) throw new Error('duplicate release names')
await mkdir(output, { recursive: true })
for (const [file, name] of files) await copyFile(file, join(output, name))
