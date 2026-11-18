import { createHash } from 'node:crypto'
import {
  createReadStream,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
  appendFileSync,
} from 'node:fs'
import { basename, join, resolve } from 'node:path'
import { execFileSync } from 'node:child_process'

const [platform, runId, version, sourceSha, directory, prefix] = process.argv.slice(2)
const fail = (message) => {
  throw new Error(`candidate ${prefix}: ${message}`)
}
if (
  !['win32', 'darwin'].includes(platform) ||
  !/^[1-9]\d*$/.test(runId || '') ||
  !/^\d+\.\d+\.\d+$/.test(version || '') ||
  !/^[a-f0-9]{40}$/.test(sourceSha || '') ||
  !directory ||
  !prefix
) {
  fail(
    'expected platform, run id, x.y.z version, lowercase source SHA, directory and output prefix',
  )
}
const artifactName = platform === 'win32' ? 'rust-bundle-win' : 'rust-bundle-mac'
const gh = (...args) => execFileSync('gh', args, { encoding: 'utf8' })
const run = JSON.parse(gh('api', `repos/${process.env.GH_REPO}/actions/runs/${runId}`))
if (String(run.id) !== runId || run.status !== 'completed' || run.conclusion !== 'success')
  fail('workflow run is not a completed successful run')
const pages = JSON.parse(
  gh(
    'api',
    '--paginate',
    '--slurp',
    `repos/${process.env.GH_REPO}/actions/runs/${runId}/artifacts?per_page=100`,
  ),
)
const artifacts = pages
  .flatMap((page) => page.artifacts)
  .filter((item) => item.name === artifactName)
if (artifacts.length !== 1 || artifacts[0].expired)
  fail(`missing, expired or ambiguous ${artifactName} artifact`)
mkdirSync(directory, { recursive: true })
gh(
  'run',
  'download',
  runId,
  '--repo',
  process.env.GH_REPO,
  '--name',
  artifactName,
  '--dir',
  directory,
)
const manifest = JSON.parse(readFileSync(join(directory, 'rust-bundle-manifest.json'), 'utf8'))
if (
  manifest.schema !== 'seuoffice-rust-bundle/v1' ||
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
const expected =
  platform === 'win32'
    ? {
        latest: {
          [`SeuOffice-Setup-${version}.exe`]: 'installer',
          [`SeuOffice-Setup-${version}.exe.blockmap`]: 'blockmap',
          'latest.yml': 'feed',
        },
        stable: {
          [`SeuOffice-Setup-${version}-stable.exe`]: 'installer',
          [`SeuOffice-Setup-${version}-stable.exe.blockmap`]: 'blockmap',
          'stable.yml': 'feed',
        },
      }
    : {
        latest: {
          [`SeuOffice-${version}-arm64.dmg`]: 'installer',
          [`SeuOffice-${version}-arm64-mac.zip`]: 'archive',
          [`SeuOffice-${version}-arm64-mac.zip.blockmap`]: 'blockmap',
          'latest-mac.yml': 'feed',
        },
        stable: {
          [`SeuOffice-${version}-arm64-stable.dmg`]: 'installer',
          [`SeuOffice-${version}-arm64-mac-stable.zip`]: 'archive',
          [`SeuOffice-${version}-arm64-mac-stable.zip.blockmap`]: 'blockmap',
          'stable-mac.yml': 'feed',
        },
      }
const signingKinds =
  platform === 'win32' ? ['unsigned', 'authenticode'] : ['self-signed', 'developer-id-notarized']
for (const channel of channels) {
  const names = expected[channel.channel]
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
    const path = resolve(directory, file.relativePath)
    if (!existsSync(path) || !statSync(path).isFile() || statSync(path).size !== file.bytes)
      fail(`missing or wrong size: ${file.relativePath}`)
    const hash = createHash('sha256')
    for await (const chunk of createReadStream(path)) hash.update(chunk)
    if (hash.digest('hex') !== file.sha256) fail(`hash mismatch: ${file.relativePath}`)
  }
}
const name =
  platform === 'win32' ? `SeuOffice-Setup-${version}.exe` : `SeuOffice-${version}-arm64-mac.zip`
const file = resolve(directory, version, name)
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
      channel: 'latest',
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
  `${prefix}=${file}\n${prefix}_version=${version}\n${prefix}_receipt=${receipt}\n`,
)
