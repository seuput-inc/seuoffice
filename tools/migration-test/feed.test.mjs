import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { parseTargets } from './targets.mjs'

const feed = new URL('feed.mjs', import.meta.url).pathname
const sha = (b) => createHash('sha512').update(b).digest('base64')

function release(version, bytes, sha512 = sha(bytes)) {
  const dir = mkdtempSync(join(tmpdir(), 'feed-'))
  const file = join(dir, `SeuOffice-Setup-${version}.exe`)
  writeFileSync(file, bytes)
  const yml = join(dir, 'latest.yml')
  writeFileSync(
    yml,
    `version: ${version}\nfiles:\n  - url: SeuOffice-Setup-${version}.exe\n    sha512: ${sha512}\n    size: ${bytes.length}\npath: SeuOffice-Setup-${version}.exe\nsha512: ${sha512}\n`,
  )
  const out = mkdtempSync(join(tmpdir(), 'feed-out-'))
  return { file, yml, out }
}

test('--yml serves the published channel file unchanged', () => {
  const r = release('0.12.43', Buffer.from('installer'))
  execFileSync('node', [feed, 'make', '--file', r.file, '--version', '0.12.43', '--out', r.out, '--yml', r.yml])
  assert.equal(readFileSync(join(r.out, 'latest.yml'), 'utf8'), readFileSync(r.yml, 'utf8'))
})

test('--yml refuses a file the published channel file does not describe', () => {
  const r = release('0.12.43', Buffer.from('installer'), 'AAAA')
  assert.throws(() =>
    execFileSync('node', [feed, 'make', '--file', r.file, '--version', '0.12.43', '--out', r.out, '--yml', r.yml], {
      stdio: 'pipe',
    }),
  )
})

test('--yml refuses another version', () => {
  const r = release('0.12.43', Buffer.from('installer'))
  assert.throws(() =>
    execFileSync('node', [feed, 'make', '--file', r.file, '--version', '0.12.44', '--out', r.out, '--yml', r.yml], {
      stdio: 'pipe',
    }),
  )
})

test('targets', () => {
  assert.deepEqual(parseTargets('{"from":["v0.12.42"],"to_tag":"v0.12.43"}'), {
    from: 'v0.12.42',
    to_tag: 'v0.12.43',
    next_tag: '',
    faults: '',
  })
  assert.throws(() => parseTargets('{"to_tag":"0.12.43"}'))
  assert.throws(() => parseTargets('{"next_tag":"v0.12.44"}'))
  assert.throws(() => parseTargets('{"faults":["boom"]}'))
})
