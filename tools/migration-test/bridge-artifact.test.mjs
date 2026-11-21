import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, join } from 'node:path'
import test from 'node:test'
import { verifyBridge } from './bridge-artifact.mjs'

const repository = 'seuput-inc/seuoffice'
const branch = 'integration/tauri-first-candidate'
const workflow = '.github/workflows/seuoffice-desktop.yml'
const sha = 'a'.repeat(40)
const invocation = `https://github.com/${repository}/actions/runs/42/attempts/2`
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex')
const platforms = {
  win32: {
    artifact: 'seuoffice-windows-x64',
    files: [
      'SeuOffice-Setup-1.2.3.exe',
      'SeuOffice-Setup-1.2.3.exe.blockmap',
      'latest.yml',
      'SeuOffice-Setup-1.2.3-stable.exe',
      'SeuOffice-Setup-1.2.3-stable.exe.blockmap',
      'stable.yml',
    ],
  },
  darwin: {
    artifact: 'seuoffice-mac-arm64',
    files: [
      'SeuOffice-1.2.3-arm64.dmg',
      'SeuOffice-1.2.3-arm64-mac.zip',
      'SeuOffice-1.2.3-arm64-mac.zip.blockmap',
      'latest-mac.yml',
      'SeuOffice-1.2.3-arm64-stable.dmg',
      'SeuOffice-1.2.3-arm64-mac-stable.zip',
      'SeuOffice-1.2.3-arm64-mac-stable.zip.blockmap',
      'stable-mac.yml',
    ],
  },
}

function fixture(t, platform = 'win32') {
  const dir = mkdtempSync(join(tmpdir(), 'bridge-artifact-'))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  const spec = platforms[platform]
  const run = {
    id: 42,
    run_attempt: 2,
    repository: { full_name: repository },
    head_repository: { full_name: repository },
    head_branch: branch,
    path: workflow,
    event: 'workflow_dispatch',
    status: 'completed',
    conclusion: 'success',
    head_sha: sha,
  }
  const artifact = {
    id: 73,
    name: spec.artifact,
    expired: false,
    expires_at: '2999-01-01T00:00:00Z',
    digest: `sha256:${'b'.repeat(64)}`,
  }
  const listing = { artifacts: [artifact], total_count: 1 }
  const subject = spec.files.map((name) => {
    writeFileSync(join(dir, name), name)
    return { name, digest: { sha256: sha256(name) } }
  })
  const certificate = { runInvocationURI: invocation }
  const verified = [{ verificationResult: { signature: { certificate }, statement: { subject } } }]
  function gh(command, ...args) {
    const flag = (key) => args[args.indexOf(key) + 1]
    assert.equal(flag('--hostname'), 'github.com')
    if (command === 'api') {
      const endpoint = `repos/${repository}/actions/runs/42`
      const path = args.find((arg) => arg.startsWith('repos/'))
      if (path === endpoint) return run
      const url = new URL(path, 'https://api.github.com/')
      assert.equal(url.pathname, `/${endpoint}/artifacts`)
      assert.equal(url.searchParams.get('name'), spec.artifact)
      return listing
    }
    assert.equal(command, 'attestation')
    assert.equal(args[0], 'verify')
    assert.equal(flag('--repo'), repository)
    assert.equal(flag('--signer-workflow'), `${repository}/${workflow}`)
    assert.equal(flag('--signer-digest'), sha)
    assert.equal(flag('--source-digest'), sha)
    assert.equal(flag('--source-ref'), `refs/heads/${branch}`)
    assert.equal(flag('--format'), 'json')
    return verified
  }
  const verify = () => verifyBridge([platform, '42', '1.2.3', sha, dir], gh)
  return { artifact, certificate, dir, listing, run, subject, verify }
}

for (const [platform, spec] of Object.entries(platforms)) {
  test(`${platform} receipt covers the attested files in both channels, not installation`, async (t) => {
    const { artifact, verify } = fixture(t, platform)
    const receipt = await verify()
    assert.equal(receipt.invocation, invocation)
    assert.equal(receipt.artifact.id, artifact.id)
    assert.equal(receipt.artifact.reportedArchiveDigest, artifact.digest)
    assert.equal(receipt.platformSigning, 'not-verified')
    assert.equal(receipt.g6, 'not-tested')
    assert.deepEqual(
      receipt.files.map((file) => basename(file.path)).sort(),
      [...spec.files].sort(),
    )
    for (const file of receipt.files) {
      const name = basename(file.path)
      assert.equal(file.channel, name.includes('stable') ? 'stable' : 'latest')
      assert.equal(file.sha256, sha256(name))
      assert.equal(file.bytes, Buffer.byteLength(name))
      assert.equal(readFileSync(file.path, 'utf8'), name)
    }
  })
}

test('rejects an unrelated dispatch even if the files and their attestations are valid', async (t) => {
  for (const changes of [
    { id: 43 },
    { repository: { full_name: 'fork/seuoffice' } },
    { head_repository: { full_name: 'fork/seuoffice' } },
    { head_branch: 'main' },
    { event: 'pull_request' },
    { path: '.github/workflows/other.yml' },
    { head_sha: 'f'.repeat(40) },
    { status: 'in_progress' },
    { conclusion: 'cancelled' },
  ]) {
    const { run, verify } = fixture(t)
    Object.assign(run, changes)
    await assert.rejects(verify, /dispatch/)
  }
})

test('requires one live artifact of the expected architecture with a reported archive digest', async (t) => {
  for (const changes of [
    { name: 'seuoffice-windows-arm64' },
    { expired: true },
    { expires_at: '2000-01-01T00:00:00Z' },
    { digest: null },
  ]) {
    const { artifact, verify } = fixture(t)
    Object.assign(artifact, changes)
    await assert.rejects(verify, /artifact/)
  }
  const { listing, verify } = fixture(t)
  listing.total_count = 2
  await assert.rejects(verify, /artifact/)
})

test('rejects an earlier attempt of the same successful run', async (t) => {
  const { certificate, verify } = fixture(t)
  certificate.runInvocationURI = invocation.replace('/attempts/2', '/attempts/1')
  await assert.rejects(verify, /attested subject/)
})

test('a stable feed cannot borrow the provenance of identical bytes under another name', async (t) => {
  const { subject, verify } = fixture(t)
  subject.find(({ name }) => name === 'stable.yml').name = 'renamed.yml'
  await assert.rejects(verify, /stable\.yml/)
})

test('rejects changed update bytes and old builds that never attested the update blockmap', async (t) => {
  const { dir, verify } = fixture(t)
  writeFileSync(join(dir, 'SeuOffice-Setup-1.2.3-stable.exe'), 'changed')
  await assert.rejects(verify, /stable\.exe/)

  const mac = fixture(t, 'darwin')
  const index = mac.subject.findIndex(({ name }) => name.endsWith('-stable.zip.blockmap'))
  mac.subject.splice(index, 1)
  await assert.rejects(mac.verify, /stable\.zip\.blockmap/)
})
