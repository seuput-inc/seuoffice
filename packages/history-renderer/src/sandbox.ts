import { execFile } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { promisify } from 'node:util'
import { HistoryError } from '@seuoffice/collab/client'

const flags = [
  'run',
  '--no-config',
  '--no-lock',
  '--no-npm',
  '--no-remote',
  '--no-code-cache',
  '--cached-only',
  '--no-prompt',
  '--deny-read',
  '--deny-write',
  '--deny-net',
  '--deny-env',
  '--deny-run',
  '--deny-ffi',
  '--deny-sys',
  '--deny-import',
  '--v8-flags=--max-old-space-size=96,--wasm-max-mem-pages=2048',
]
const require = createRequire(import.meta.url)
const exec = promisify(execFile)

export async function launch(program: URL, input: Uint8Array): Promise<Buffer> {
  const target =
    process.platform === 'darwin'
      ? 'darwin-arm64'
      : process.platform === 'linux'
        ? 'linux-arm64-glibc'
        : undefined
  if (!target || process.arch !== 'arm64')
    throw new HistoryError('unsupported', 'sandbox requires arm64 Linux glibc or macOS')
  let bin
  try {
    bin = require.resolve(`@deno/${target}/deno`)
  } catch {
    throw new HistoryError('unsupported', 'sandbox runtime is not installed')
  }
  const dir = await mkdtemp(join(tmpdir(), 'seuoffice-history-'))
  try {
    const task = exec(bin, [...flags, fileURLToPath(program)], {
      encoding: 'buffer',
      cwd: dir,
      env: { DENO_DIR: dir, HOME: dir, DENO_NO_UPDATE_CHECK: '1' },
      timeout: 15000,
      killSignal: 'SIGKILL',
      maxBuffer: 3 * 1024 * 1024 + 4,
    })
    task.child.stdin!.on('error', () => {
      /* execFile reports early termination. */
    })
    task.child.stdin!.end(input)
    return (await task).stdout
  } catch {
    throw new HistoryError('unsupported', 'sandbox failed or exceeded its limits')
  } finally {
    await rm(dir, { recursive: true, force: true })
  }
}
