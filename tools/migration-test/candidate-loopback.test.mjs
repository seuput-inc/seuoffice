import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, dirname, join } from 'node:path'
import test from 'node:test'
import { fileURLToPath, pathToFileURL } from 'node:url'

const bundle = new URL('../../crates/shell/bundle/', import.meta.url)
const selector = fileURLToPath(new URL('candidate.mjs', import.meta.url))
const sha = 'a'.repeat(40)
const repository = 'seuput-inc/seuoffice'
const branch = 'integration/tauri-first-candidate'
const workflow = '.github/workflows/seuoffice-migration.yml'
const signerWorkflow = '.github/workflows/seuoffice-rust-bundle.yml'
const invocation = `https://github.com/${repository}/actions/runs/42/attempts/2`
const manifestName = 'rust-qa-loopback-manifest.json'
const versions = ['1.2.3', '1.2.4']
const digest = (bytes, algorithm = 'sha256', encoding = 'hex') =>
  createHash(algorithm).update(bytes).digest(encoding)

function scratch(t) {
  const dir = mkdtempSync(join(tmpdir(), 'tauri-feed-test-'))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  return dir
}

function run(args, env = {}) {
  const child = spawnSync(process.execPath, args, {
    encoding: 'utf8',
    env: { ...process.env, ...env },
  })
  if (child.error) throw child.error
  return child
}

function ok(child) {
  assert.equal(child.status, 0, child.stderr || child.stdout)
}

test('packaging writes only the requested feed; a new release build removes the QA address', (t) => {
  const root = scratch(t)
  const script = join(root, 'crates/shell/bundle/release-feed.mjs')
  mkdirSync(dirname(script), { recursive: true })
  mkdirSync(join(root, 'apps/shell'), { recursive: true })
  copyFileSync(new URL('release-feed.mjs', bundle), script)
  copyFileSync(
    new URL('../../../apps/shell/update-repository.json', bundle),
    join(root, 'apps/shell/update-repository.json'),
  )
  for (const channel of ['latest', 'stable']) {
    const env = { SEUOFFICE_UPDATE_CHANNEL: channel }
    const output = () => readFileSync(join(dirname(script), 'out/app-update.yml'), 'utf8')
    ok(run([script, 'app-update', '--mode', 'loopback'], env))
    assert.equal(
      output(),
      `provider: generic\nurl: http://127.0.0.1:8765/\nchannel: ${channel}\nupdaterCacheDirName: seuoffice-updater\n`,
    )
    ok(run([script, 'app-update'], env))
    assert.match(output(), /provider: github\n/)
    assert.doesNotMatch(output(), /127\.0\.0\.1|provider: generic/)
  }
  assert.notEqual(run([script, 'app-update', '--mode', 'https://example.invalid']).status, 0)
})

function buildFixture(root, platform) {
  const mac = platform === 'darwin'
  const subjects = []
  for (const version of versions) {
    const dir = join(root, version)
    mkdirSync(dir, { recursive: true })
    const put = (name, contents) => {
      writeFileSync(join(dir, name), contents)
      subjects.push({ name, digest: { sha256: digest(contents) } })
    }
    for (const channel of ['latest', 'stable']) {
      const suffix = channel === 'stable' ? '-qa-loopback-stable' : '-qa-loopback'
      const update = mac
        ? `SeuOffice-${version}-arm64-mac${suffix}.zip`
        : `SeuOffice-Setup-${version}${suffix}.exe`
      const payloads = mac ? [update, `SeuOffice-${version}-arm64${suffix}.dmg`] : [update]
      const files = payloads.map((name) => {
        put(name, name)
        return {
          url: name,
          size: Buffer.byteLength(name),
          sha512: digest(name, 'sha512', 'base64'),
        }
      })
      put(`${update}.blockmap`, `map ${update}`)
      put(
        `${channel}${mac ? '-mac' : ''}.yml`,
        JSON.stringify({ version, files, path: update, sha512: files[0].sha512 }),
      )
    }
  }
  const produce = (mode) =>
    run([
      fileURLToPath(new URL('bundle-manifest.mjs', bundle)),
      '--mode',
      mode,
      '--root',
      root,
      '--platform',
      platform,
      '--versions',
      versions.join(' '),
      '--source',
      sha,
      '--definition',
      sha,
      '--run',
      '42',
      '--signing',
      mac ? 'ad-hoc' : 'unsigned',
    ])
  ok(produce('loopback'))
  assert.notEqual(produce('release').status, 0, 'release manifest accepted QA payloads')
  const manifest = readFileSync(join(root, manifestName))
  subjects.push({ name: manifestName, digest: { sha256: digest(manifest) } })
  return subjects
}

// Replace only the child's gh transport. An unexpected command throws instead of using the network.
const transport = `
import child from 'node:child_process'
import { cpSync, readFileSync } from 'node:fs'
import { syncBuiltinESMExports } from 'node:module'
const { run, listing, attestation, artifactDir, dest, expectedVerify } = JSON.parse(readFileSync(process.env.QA_RESPONSES))
child.execFileSync = (exe, args) => {
  if (exe !== 'gh') throw Error(exe)
  if (args[0] === 'api') return JSON.stringify(args.includes('--slurp') ? [listing] : run)
  if (args[0] === 'run' && args[1] === 'download' && args.at(-1) === dest) {
    cpSync(artifactDir, dest, {recursive:true, force:false, errorOnExist:true}); return ''
  }
  if (JSON.stringify(args) === JSON.stringify(expectedVerify)) return JSON.stringify(attestation)
  throw Error('unexpected gh invocation: ' + JSON.stringify(args))
}
syncBuiltinESMExports()
`

for (const platform of ['win32', 'darwin']) {
  test(`${platform}: select exact attested bytes and reject reuse with tampering or another attempt`, (t) => {
    const dir = scratch(t)
    const artifactDir = join(dir, 'published')
    const dest = join(dir, 'download')
    const output = join(dir, 'outputs')
    const responses = join(dir, 'responses.json')
    const preload = join(dir, 'gh.mjs')
    writeFileSync(preload, transport)
    const subjects = buildFixture(artifactDir, platform)
    const mac = platform === 'darwin'
    const result = {
      signature: { certificate: { runInvocationURI: invocation } },
      statement: { subject: subjects },
    }
    const api = {
      artifactDir,
      dest,
      run: {
        id: 42,
        run_attempt: 2,
        status: 'completed',
        conclusion: 'success',
        event: 'workflow_dispatch',
        head_branch: branch,
        head_sha: sha,
        path: workflow,
        repository: { full_name: repository },
        head_repository: { full_name: repository },
      },
      listing: {
        artifacts: [
          {
            name: `rust-qa-loopback-${mac ? 'mac' : 'win'}`,
            id: 123,
            digest: `sha256:${'b'.repeat(64)}`,
            expired: false,
            expires_at: '2999-01-01T00:00:00Z',
          },
        ],
      },
      attestation: [{ verificationResult: result }],
      expectedVerify: [
        'attestation',
        'verify',
        join(dest, manifestName),
        '--hostname',
        'github.com',
        '--repo',
        repository,
        '--signer-workflow',
        `${repository}/${signerWorkflow}`,
        '--signer-digest',
        sha,
        '--source-digest',
        sha,
        '--source-ref',
        `refs/heads/${branch}`,
        '--format',
        'json',
      ],
    }
    const select = (version, prefix, channel) => {
      writeFileSync(responses, JSON.stringify(api))
      return run(
        [
          '--import',
          pathToFileURL(preload).href,
          selector,
          platform,
          '42',
          version,
          sha,
          dest,
          prefix,
          'loopback',
          channel,
        ],
        { GH_REPO: repository, GITHUB_OUTPUT: output, QA_RESPONSES: responses },
      )
    }
    ok(select('1.2.3', 'first', 'latest'))
    // No second download is possible. Selection still checks the new version's actual bytes.
    rmSync(artifactDir, { recursive: true })
    ok(select('1.2.4', 'next', 'stable'))
    const record = JSON.parse(readFileSync(join(dest, 'next-receipt.json')))
    assert.equal(record.promotable, false)
    assert.equal(record.invocation, invocation)
    assert.equal(record.channel, 'stable')
    assert.equal(
      basename(record.file),
      mac
        ? 'SeuOffice-1.2.4-arm64-mac-qa-loopback-stable.zip'
        : 'SeuOffice-Setup-1.2.4-qa-loopback-stable.exe',
    )
    const yml = join(dest, '1.2.4', mac ? 'stable-mac.yml' : 'stable.yml')
    assert.equal(record.yml, yml)
    assert.ok(readFileSync(output, 'utf8').split('\n').includes(`next_yml=${yml}`))
    api.run.run_attempt++
    assert.match(select('1.2.4', 'wrong', 'stable').stderr, /not attested by this run attempt/)
    api.run.run_attempt--
    result.statement.subject = subjects.filter((s) => s.name !== basename(record.file))
    assert.match(select('1.2.4', 'missing', 'stable').stderr, /not attested with its manifest/)
    result.statement.subject = subjects
    writeFileSync(record.file, Buffer.alloc(readFileSync(record.file).length))
    assert.match(select('1.2.4', 'changed', 'stable').stderr, /hash mismatch/)
  })
}
