// Verify files already present in a desktop workflow artifact. No download or extraction.
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { createReadStream, lstatSync } from 'node:fs'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const repository = 'seuput-inc/seuoffice'
const branch = 'integration/tauri-first-candidate'
const sourceRef = `refs/heads/${branch}`
const workflow = '.github/workflows/seuoffice-desktop.yml'
const usage =
  'usage: node bridge-artifact.mjs <win32|darwin> <run-id> <x.y.z> <source-sha> <artifact-dir>'

function ghJson(...args) {
  return JSON.parse(
    execFileSync('gh', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] }),
  )
}

export async function verifyBridge(args, gh = ghJson) {
  const [platform, runId, version, sourceSha, directory] = args
  if (
    args.length !== 5 ||
    !['win32', 'darwin'].includes(platform) ||
    !/^[1-9]\d*$/.test(runId) ||
    !/^\d+\.\d+\.\d+$/.test(version) ||
    !/^[a-f0-9]{40}$/.test(sourceSha) ||
    !directory
  ) {
    throw new Error(usage)
  }

  const endpoint = `repos/${repository}/actions/runs/${runId}`
  const run = gh('api', '--hostname', 'github.com', endpoint)
  if (
    String(run.id) !== runId ||
    run.repository?.full_name !== repository ||
    run.head_repository?.full_name !== repository ||
    run.event !== 'workflow_dispatch' ||
    run.head_branch !== branch ||
    run.path !== workflow ||
    run.head_sha !== sourceSha ||
    run.status !== 'completed' ||
    run.conclusion !== 'success' ||
    !Number.isSafeInteger(run.run_attempt) ||
    run.run_attempt < 1
  ) {
    throw new Error(
      'expected a successful candidate branch dispatch at the specified workflow and SHA',
    )
  }
  const invocation = `https://github.com/${repository}/actions/runs/${runId}/attempts/${run.run_attempt}`

  const mac = platform === 'darwin'
  const name = mac ? 'seuoffice-mac-arm64' : 'seuoffice-windows-x64'
  const listing = gh(
    'api',
    '--hostname',
    'github.com',
    `${endpoint}/artifacts?name=${name}&per_page=100`,
  )
  const artifact = listing.artifacts?.[0]
  if (
    listing.total_count !== 1 ||
    listing.artifacts?.length !== 1 ||
    artifact?.name !== name ||
    artifact.expired !== false ||
    !(Date.parse(artifact.expires_at) > Date.now()) ||
    !Number.isSafeInteger(artifact.id) ||
    artifact.id < 1 ||
    !/^sha256:[a-f0-9]{64}$/.test(artifact.digest)
  ) {
    throw new Error(`missing, expired, ambiguous or incomplete ${name} artifact`)
  }

  const files = []
  for (const channel of ['latest', 'stable']) {
    const suffix = channel === 'stable' ? '-stable' : ''
    const update = mac
      ? `SeuOffice-${version}-arm64-mac${suffix}.zip`
      : `SeuOffice-Setup-${version}${suffix}.exe`
    const expected = {
      ...(mac
        ? { installer: `SeuOffice-${version}-arm64${suffix}.dmg`, archive: update }
        : { installer: update }),
      blockmap: `${update}.blockmap`,
      feed: `${channel}${mac ? '-mac' : ''}.yml`,
    }
    for (const [kind, filename] of Object.entries(expected)) {
      const path = resolve(directory, filename)
      if (!lstatSync(path).isFile()) throw new Error(`expected a regular file: ${path}`)
      const hash = createHash('sha256')
      let bytes = 0
      for await (const chunk of createReadStream(path)) {
        hash.update(chunk)
        bytes += chunk.length
      }
      if (!bytes) throw new Error(`empty file: ${path}`)
      const sha256 = hash.digest('hex')
      const results = gh(
        'attestation',
        'verify',
        path,
        '--hostname',
        'github.com',
        '--repo',
        repository,
        '--signer-workflow',
        `${repository}/${workflow}`,
        '--signer-digest',
        sourceSha,
        '--source-digest',
        sourceSha,
        '--source-ref',
        sourceRef,
        '--format',
        'json',
      )
      // The certificate binds the run attempt. The signed subject binds the bytes to the filename.
      const attested = results.some(
        ({ verificationResult: result }) =>
          result?.signature?.certificate?.runInvocationURI === invocation &&
          result.statement?.subject?.some(
            (subject) => subject.name === filename && subject.digest?.sha256 === sha256,
          ),
      )
      if (!attested) throw new Error(`no attested subject for ${filename} in ${invocation}`)
      files.push({ channel, kind, path, bytes, sha256 })
    }
  }
  return {
    schema: 'seuoffice-electron-bridge-provenance/v1',
    g6: 'not-tested',
    platformSigning: 'not-verified',
    repository,
    workflow,
    sourceRef,
    sourceSha,
    runId,
    runAttempt: run.run_attempt,
    invocation,
    platform,
    arch: mac ? 'arm64' : 'x64',
    version,
    // This is the API's digest for the outer Actions archive. Only files[] was verified locally.
    artifact: {
      id: artifact.id,
      name,
      reportedArchiveDigest: artifact.digest,
      expiresAt: artifact.expires_at,
    },
    files,
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  try {
    console.log(JSON.stringify(await verifyBridge(process.argv.slice(2)), null, 2))
  } catch (error) {
    console.error(error.message)
    process.exitCode = 1
  }
}
