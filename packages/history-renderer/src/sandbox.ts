import { execFile, spawn } from 'node:child_process'
import { once } from 'node:events'
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
const runtimes = ['darwin-arm64', 'linux-arm64-glibc', 'linux-x64-glibc']

/** The Deno package for this host; a musl host would only fail to exec the glibc binary. */
export function denoPackage(
  platform: string = process.platform,
  arch: string = process.arch,
  glibc: () => string | undefined = () => {
    // @types/node declares the diagnostic report as a bare object.
    const report = process.report.getReport() as { header?: { glibcVersionRuntime?: string } }
    return report.header?.glibcVersionRuntime
  },
): string {
  const target = platform === 'linux' ? `linux-${arch}-glibc` : `${platform}-${arch}`
  if (!runtimes.includes(target))
    throw new HistoryError(
      'unsupported',
      `sandbox has no runtime for ${platform} ${arch}; it requires Linux glibc on arm64 or x64, or macOS arm64`,
    )
  if (platform === 'linux' && !glibc())
    throw new HistoryError(
      'unsupported',
      `sandbox requires glibc on Linux ${arch}; musl is not supported`,
    )
  return `@deno/${target}`
}

async function sandbox<T>(
  program: URL,
  run: (
    bin: string,
    args: string[],
    options: { cwd: string; env: NodeJS.ProcessEnv },
  ) => Promise<T>,
): Promise<T> {
  const name = denoPackage()
  let bin
  try {
    bin = require.resolve(`${name}/deno`)
  } catch {
    throw new HistoryError('unsupported', 'sandbox runtime is not installed')
  }
  const dir = await mkdtemp(join(tmpdir(), 'seuoffice-history-'))
  try {
    return await run(bin, [...flags, fileURLToPath(program)], {
      cwd: dir,
      env: { DENO_DIR: dir, HOME: dir, DENO_NO_UPDATE_CHECK: '1' },
    })
  } finally {
    await rm(dir, { recursive: true, force: true })
  }
}

export async function launch(program: URL, input: Uint8Array): Promise<Buffer> {
  return sandbox(program, async (bin, args, options) => {
    try {
      const task = exec(bin, args, {
        ...options,
        encoding: 'buffer',
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
      throw sandboxFailure()
    }
  })
}

// A crash, timeout or resource limit says nothing about the history: the same request may succeed.
function sandboxFailure() {
  return new HistoryError('unavailable', 'sandbox failed or exceeded its limits')
}

export async function launchStream(
  mode: 'rgb' | 'mixed',
  remainingMs: () => number,
  input: (write: (bytes: Uint8Array) => Promise<void>, signal: AbortSignal) => Promise<void>,
  signal?: AbortSignal,
): Promise<Buffer> {
  const program = new URL(mode === 'rgb' ? 'stream-guest.js' : 'mixed-guest.js', import.meta.url)
  return sandbox(program, async (bin, args, options) => {
    signal?.throwIfAborted()
    const child = spawn(bin, args, {
      ...options,
      stdio: ['pipe', 'pipe', 'ignore'],
      timeout: Math.min(15000, remainingMs()),
      killSignal: 'SIGKILL',
    })
    const controller = new AbortController()
    const cancel = () => {
      child.kill('SIGKILL')
      controller.abort(signal?.reason)
    }
    signal?.addEventListener('abort', cancel, { once: true })
    const limit = 3 * 1024 * 1024
    const chunks: Buffer[] = []
    let size = 0
    child.stdout.on('data', (chunk: Buffer) => {
      size += chunk.length
      if (size <= limit) chunks.push(chunk)
      else child.kill('SIGKILL')
    })
    const closed = once(child, 'close').then(
      ([code]) => {
        if (code !== 0 || size > limit) throw sandboxFailure()
        return Buffer.concat(chunks, size)
      },
      () => {
        throw sandboxFailure()
      },
    )
    // A guest can reject the header before reading the rest of stdin.
    const pipeClosed = Symbol('stdin closed')
    child.stdin.on('error', () => {})
    const sending = input(
      (bytes) =>
        new Promise<void>((resolve, reject) => {
          child.stdin.write(bytes, (err) => (err ? reject(pipeClosed) : resolve()))
        }),
      controller.signal,
    ).then(
      () => child.stdin.end(),
      (err) => {
        if (err !== pipeClosed) throw err
      },
    )
    try {
      await Promise.race([sending, closed])
      return await closed
    } finally {
      signal?.removeEventListener('abort', cancel)
      controller.abort()
      child.stdin.destroy()
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL')
      await closed.catch(() => {})
    }
  })
}
