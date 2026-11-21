import { createHash } from 'node:crypto'
import {
  createReadStream,
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
  appendFileSync,
} from 'node:fs'
import { basename, join, resolve } from 'node:path'
import { execFileSync } from 'node:child_process'

const [
  platform,
  runId,
  version,
  sourceSha,
  directory,
  prefix,
  mode = 'release',
  selected = 'latest',
] = process.argv.slice(2)
const loopback = mode === 'loopback'
const mac = platform === 'darwin'
const base = loopback ? 'rust-qa-loopback' : 'rust-bundle'
const repository = process.env.GH_REPO
const workflow = '.github/workflows/seuoffice-migration.yml'
const signerWorkflow = '.github/workflows/seuoffice-rust-bundle.yml'
const qaBranch = 'integration/tauri-first-candidate'
const fail = (message) => {
  throw new Error(`candidate ${prefix}: ${message}`)
}
if (
  !['win32', 'darwin'].includes(platform) ||
  !/^[1-9]\d*$/.test(runId || '') ||
  !/^\d+\.\d+\.\d+$/.test(version || '') ||
  !/^[a-f0-9]{40}$/.test(sourceSha || '') ||
  !directory ||
  !/^[a-z][a-z0-9_]*$/.test(prefix || '') ||
  !['release', 'loopback'].includes(mode) ||
  !['latest', 'stable'].includes(selected) ||
  (loopback && (repository !== 'seuput-inc/seuoffice' || process.argv.length !== 10)) ||
  process.argv.length > 10
) {
  fail(
    'expected platform, run id, x.y.z version, lowercase source SHA, directory, prefix, [release|loopback latest|stable]',
  )
}
const artifactName = `${base}-${mac ? 'mac' : 'win'}`
const gh = (...args) =>
  execFileSync('gh', args, {
    encoding: 'utf8',
    env: loopback ? { ...process.env, GH_HOST: 'github.com' } : process.env,
  })
const run = JSON.parse(gh('api', `repos/${repository}/actions/runs/${runId}`))
if (String(run.id) !== runId || run.status !== 'completed' || run.conclusion !== 'success')
  fail('workflow run is not a completed successful run')
if (
  loopback &&
  (run.repository?.full_name !== repository ||
    run.head_repository?.full_name !== repository ||
    run.path !== workflow ||
    run.event !== 'workflow_dispatch' ||
    run.head_branch !== qaBranch ||
    run.head_sha !== sourceSha ||
    !Number.isSafeInteger(run.run_attempt) ||
    run.run_attempt < 1)
)
  fail('not a QA producer dispatch at the specified SHA')
const invocation = `https://github.com/${repository}/actions/runs/${runId}/attempts/${run.run_attempt}`
const pages = JSON.parse(
  gh(
    'api',
    '--paginate',
    '--slurp',
    `repos/${repository}/actions/runs/${runId}/artifacts?per_page=100`,
  ),
)
const artifacts = pages
  .flatMap((page) => page.artifacts)
  .filter((item) => item.name === artifactName)
if (
  artifacts.length !== 1 ||
  artifacts[0].expired !== false ||
  (loopback &&
    (!Number.isSafeInteger(artifacts[0].id) ||
      artifacts[0].id < 1 ||
      !(Date.parse(artifacts[0].expires_at) > Date.now()) ||
      !/^sha256:[a-f0-9]{64}$/.test(artifacts[0].digest || '')))
)
  fail(`missing, expired or ambiguous ${artifactName} artifact`)
mkdirSync(directory, { recursive: true })
if (!lstatSync(directory).isDirectory()) fail('download directory is not a plain directory')
if (!loopback || !readdirSync(directory).length) {
  gh('run', 'download', runId, '--repo', repository, '--name', artifactName, '--dir', directory)
}
const manifestName = `${base}-manifest.json`
const manifestPath = join(directory, manifestName)
if (!lstatSync(manifestPath).isFile()) fail('expected a regular manifest')
const manifestBytes = readFileSync(manifestPath)
const manifestDigest = createHash('sha256').update(manifestBytes).digest('hex')
let subjects
if (loopback) {
  // The producer attests the manifest and every payload together, in one statement.
  const attestations = JSON.parse(
    gh(
      'attestation',
      'verify',
      manifestPath,
      '--hostname',
      'github.com',
      '--repo',
      repository,
      '--signer-workflow',
      `${repository}/${signerWorkflow}`,
      '--signer-digest',
      sourceSha,
      '--source-digest',
      sourceSha,
      '--source-ref',
      `refs/heads/${qaBranch}`,
      '--format',
      'json',
    ),
  )
  subjects = attestations
    .map(({ verificationResult }) => verificationResult)
    .find(
      (result) =>
        result?.signature?.certificate?.runInvocationURI === invocation &&
        result.statement?.subject?.some(
          (s) => s.name === manifestName && s.digest?.sha256 === manifestDigest,
        ),
    )?.statement.subject
  if (!subjects) fail('manifest was not attested by this run attempt')
}
const manifest = JSON.parse(manifestBytes)
if (
  manifest.schema !== (loopback ? 'seuoffice-rust-qa-loopback/v1' : 'seuoffice-rust-bundle/v1') ||
  (loopback &&
    (manifest.mode !== mode ||
      manifest.promotable !== false ||
      manifest.feedBase !== 'http://127.0.0.1:8765/' ||
      manifest.versions?.length !== 2)) ||
  manifest.sourceSha !== sourceSha ||
  manifest.definitionSha !== run.head_sha ||
  manifest.workflowRunId !== runId ||
  manifest.platform !== platform ||
  manifest.arch !== (platform === 'win32' ? 'x64' : 'arm64')
)
  fail('manifest does not match expected source, run or platform')
const versions = Array.isArray(manifest.versions)
  ? manifest.versions.filter((item) => item.version === version)
  : []
if (versions.length !== 1 || !Array.isArray(versions[0].channels))
  fail('missing or ambiguous version')
const channels = versions[0].channels
if (
  channels.length !== 2 ||
  !['latest', 'stable'].every(
    (name) => channels.filter((item) => item.channel === name).length === 1,
  )
)
  fail('missing or ambiguous channels')
const versionDir = resolve(directory, version)
if (!lstatSync(versionDir).isDirectory()) fail('version is not a plain directory')
const signingKinds = mac ? ['ad-hoc'] : loopback ? ['unsigned'] : ['unsigned', 'authenticode']
const allowedFiles = new Set()
for (const channel of channels) {
  const suffix = `${loopback ? '-qa-loopback' : ''}${channel.channel === 'stable' ? '-stable' : ''}`
  const payload = mac
    ? `SeuOffice-${version}-arm64-mac${suffix}.zip`
    : `SeuOffice-Setup-${version}${suffix}.exe`
  const names = {
    [payload]: mac ? 'archive' : 'installer',
    [`${payload}.blockmap`]: 'blockmap',
    [`${channel.channel}${mac ? '-mac' : ''}.yml`]: 'feed',
    ...(mac && { [`SeuOffice-${version}-arm64${suffix}.dmg`]: 'installer' }),
  }
  if (
    !signingKinds.includes(channel.signing) ||
    !Array.isArray(channel.files) ||
    channel.files.length !== Object.keys(names).length
  )
    fail(`invalid ${channel.channel} files or signing`)
  const seen = new Set()
  for (const file of channel.files) {
    if (
      !Object.hasOwn(names, file.name) ||
      seen.has(file.name) ||
      basename(file.name) !== file.name ||
      file.relativePath !== `${version}/${file.name}` ||
      file.kind !== names[file.name] ||
      !/^[a-f0-9]{64}$/.test(file.sha256 || '') ||
      !Number.isSafeInteger(file.bytes) ||
      file.bytes <= 0
    )
      fail(`invalid file entry ${file.name}`)
    seen.add(file.name)
    allowedFiles.add(file.name)
    const path = resolve(directory, file.relativePath)
    const info = lstatSync(path)
    if (!info.isFile() || info.size !== file.bytes)
      fail(`missing or wrong size: ${file.relativePath}`)
    const hash = createHash('sha256')
    for await (const chunk of createReadStream(path)) hash.update(chunk)
    if (hash.digest('hex') !== file.sha256) fail(`hash mismatch: ${file.relativePath}`)
    if (loopback && !subjects.some((s) => s.name === file.name && s.digest?.sha256 === file.sha256))
      fail(`payload was not attested with its manifest: ${file.relativePath}`)
  }
}
if (loopback && readdirSync(versionDir).some((name) => !allowedFiles.has(name)))
  fail('unexpected file in the verified version directory')
const channel = channels.find((item) => item.channel === selected)
const pathFor = (kind) =>
  resolve(directory, channel.files.find((entry) => entry.kind === kind).relativePath)
const file = pathFor(mac ? 'archive' : 'installer')
const yml = pathFor('feed')
const receipt = resolve(directory, `${prefix}-receipt.json`)
writeFileSync(
  receipt,
  JSON.stringify(
    {
      runId,
      definitionSha: manifest.definitionSha,
      sourceSha,
      platform,
      arch: manifest.arch,
      version,
      channel: selected,
      ...(loopback && {
        mode,
        promotable: false,
        schema: 'seuoffice-rust-qa-selection/v1',
        feedBase: manifest.feedBase,
        workflow,
        signerWorkflow,
        runAttempt: run.run_attempt,
        invocation,
        manifestSha256: manifestDigest,
        artifactId: artifacts[0].id,
        reportedArchiveDigest: artifacts[0].digest,
        yml,
      }),
      artifact: artifactName,
      file,
      channels,
    },
    null,
    2,
  ) + '\n',
)
appendFileSync(
  process.env.GITHUB_OUTPUT,
  `${prefix}=${file}\n${prefix}_version=${version}\n${prefix}_receipt=${receipt}\n` +
    (loopback ? `${prefix}_yml=${yml}\n` : ''),
)
