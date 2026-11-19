// The guard that keeps the e2e-replay build out of a bundle, on files built here.
//   cd crates/shell/ci && npx vitest run replay-guard.test.mjs

import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'

const script = fileURLToPath(new URL('./replay-guard.sh', import.meta.url))
const BINARY = Buffer.from([0, 1, 2, 255, 10, 0, 200])
const RECORD = Buffer.from('native-ipc.jsonl')
const MARKER = Buffer.from('e2eReplay')

let dir
beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), 'replay-guard-'))
})
afterEach(() => {
  fs.rmSync(dir, { recursive: true, force: true })
})

function guard(mode, ...parts) {
  const exe = path.join(dir, 'SeuOffice')
  if (parts.length > 0) fs.writeFileSync(exe, Buffer.concat(parts))
  const run = spawnSync('bash', [script, mode, exe], { encoding: 'utf8' })
  return { status: run.status, text: run.stdout + run.stderr }
}

// node on Windows finds the WSL launcher for `bash` before Git Bash; the workflow steps that use
// the guard there run it in their own Git Bash shell
describe.skipIf(process.platform === 'win32')('replay-guard.sh', () => {
  it('passes an executable that ships only without the marker', () => {
    expect(guard('absent', BINARY, RECORD, BINARY)).toMatchObject({ status: 0 })
    const shipped = guard('absent', BINARY, RECORD, BINARY, MARKER, BINARY)
    expect(shipped.status).toBe(1)
    expect(shipped.text).toContain('must not ship')
  })

  it('passes the e2e build only with the marker', () => {
    expect(guard('present', BINARY, MARKER, BINARY, RECORD)).toMatchObject({ status: 0 })
    const plain = guard('present', BINARY, RECORD, BINARY)
    expect(plain.status).toBe(1)
    expect(plain.text).toContain('lacks the e2e-replay feature')
  })

  it('does not take the feature name or a split marker for the marker', () => {
    const near = Buffer.from('e2e-replay e2e replay e2eRepla\0y unresolvedReplay')
    expect(guard('absent', BINARY, RECORD, near).status).toBe(0)
    expect(guard('present', BINARY, RECORD, near).status).toBe(1)
  })

  it('fails closed on a file that is missing or is not the shell', () => {
    const missing = guard('absent')
    expect(missing.status).toBe(1)
    expect(missing.text).toContain('there is no executable')
    const other = guard('absent', BINARY)
    expect(other.status).toBe(1)
    expect(other.text).toContain('not the shell')
    expect(guard('present', BINARY, MARKER).status).toBe(1)
    expect(guard('ship', BINARY, RECORD).status).toBe(2)
  })
})
