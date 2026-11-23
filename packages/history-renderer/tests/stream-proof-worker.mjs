import assert from 'node:assert/strict'
import { channel } from 'node:diagnostics_channel'
import { createReadStream, existsSync, readFileSync } from 'node:fs'
import { open, readFile, rename, unlink } from 'node:fs/promises'
import { dirname } from 'node:path'
import { createInterface } from 'node:readline'
import { renderBytes, renderStream } from '@seuoffice/history-renderer'

// utime/stime of waited-for children, summed across guest threads. This fresh host has just one child.
function childTicks() {
  const stat = readFileSync('/proc/self/stat', 'ascii')
  const fields = stat.slice(stat.lastIndexOf(') ') + 2).split(' ')
  return Number(fields[13]) + Number(fields[14])
}

async function run(job) {
  const { input, history, header, take, output, fault, legacy } = job
  const partial = output + '.partial'
  const fd = await open(partial, 'wx')
  let aborts = 0,
    stagedBytes = 0,
    maxWrite = 0,
    frameCount = 0,
    frameBytes = 0
  let guest, guestLifetimeMs
  channel('child_process').subscribe(({ process: child }) => {
    guest = child
    const start = performance.now()
    child.once('close', () => {
      guestLifetimeMs = performance.now() - start
    })
  })
  const reads = []
  const stage = {
    async write(bytes) {
      maxWrite = Math.max(maxWrite, bytes.length)
      for (let at = 0; at < bytes.length;) {
        const { bytesWritten } = await fd.write(bytes, at, bytes.length - at)
        assert(bytesWritten > 0)
        at += bytesWritten
        stagedBytes += bytesWritten
      }
    },
    async abort() {
      aborts++
      await fd.close()
      await unlink(partial)
    },
  }
  const source = {
    byteLength: job.declaredBytes ?? input.size + (fault === 'wrong-size' ? 1 : 0),
    async *open() {
      const pass = reads.push(0) - 1
      for await (const bytes of createReadStream(input.path, { highWaterMark: 64 * 1024 })) {
        reads[pass] += bytes.length
        if (fault === 'changed' && pass === 1 && reads[pass] === input.size)
          bytes[bytes.length - 1] ^= 1
        yield bytes
      }
    },
  }
  async function* frames() {
    const stream = createReadStream(history, { highWaterMark: 64 * 1024 })
    const lines = createInterface({ input: stream, crlfDelay: Infinity })
    let v = 0
    try {
      for await (const line of lines) {
        v++
        if (!(fault === 'missing-first' && v === 1)) {
          const bytes = Buffer.from(line)
          frameCount++
          frameBytes += bytes.length
          yield bytes
        }
        if (v === take) break
      }
    } finally {
      lines.close()
      stream.destroy()
    }
  }
  const headerBytes = Buffer.from(JSON.stringify(header))
  let oldBase, oldRequest
  if (legacy) {
    assert(input.size < 1024 * 1024 && take < 128)
    oldBase = await readFile(input.path)
    const changes = []
    for await (const b of frames()) changes.push(JSON.parse(b))
    oldRequest = Buffer.from(JSON.stringify({ ...header, changes }))
    assert(oldRequest.length <= 1024 * 1024)
  }
  let result, failure
  const ticks = childTicks()
  const cpu = process.cpuUsage()
  const start = performance.now()
  try {
    if (legacy) {
      const { bytes, ...meta } = await renderBytes(oldBase, oldRequest)
      await stage.write(bytes)
      result = meta
    } else result = await renderStream(source, headerBytes, frames(), stage)
    assert.equal(result.fileName, 'history.pdf')
    await fd.sync()
    await fd.close()
    await rename(partial, output)
    const dir = await open(dirname(output), 'r')
    try {
      await dir.sync()
    } finally {
      await dir.close()
    }
  } catch (error) {
    if (!job.expectedError || error.code !== job.expectedError) throw error
    failure = error.code
  }
  const elapsedMs = performance.now() - start
  const host = process.cpuUsage(cpu)
  const guestCpuTicks = childTicks() - ticks
  const processPeakKiB = process.resourceUsage().maxRSS
  const jsonBytes =
    headerBytes.length + ',"changes":[]'.length + Math.max(0, frameCount - 1) + frameBytes
  if (job.expectedError) {
    assert.equal(failure, job.expectedError)
    assert.equal(aborts, 1)
    assert(!existsSync(partial) && !existsSync(output))
    if (fault === 'changed') {
      assert.deepEqual(reads, [input.size, input.size])
      assert(stagedBytes > 0)
    }
    if (fault === 'declared-cap') {
      assert.deepEqual(reads, [])
      assert.equal(guest, undefined)
    }
    if (fault === 'history-cap') {
      assert.equal(frameCount, take)
      assert(jsonBytes > 32 * 1024 * 1024)
    }
  } else {
    assert.equal(aborts, 0)
    assert(stagedBytes > 0 && stagedBytes <= 50 * 1024 * 1024)
    if (!legacy) assert.deepEqual(reads, [input.size, input.size])
    assert.equal(frameCount, take)
    assert(jsonBytes <= 32 * 1024 * 1024)
  }
  const legacyWholeFrameBytes = legacy ? 4 + input.size + oldRequest.length : undefined
  const stdoutBytes = guest?.stdout?.bytesRead
  let confinement
  if (!failure) {
    assert(
      stdoutBytes > 0 && stdoutBytes <= 3 * 1024 * 1024 + (legacy ? 4 : 0),
      'guest stdout byte counter',
    )
    if (legacy)
      assert.equal(stdoutBytes, 4 + Buffer.byteLength(JSON.stringify(result)) + stagedBytes)
    if (!legacy) {
      const args = guest.spawnargs
      confinement = Object.fromEntries(
        [
          '--deny-read',
          '--deny-write',
          '--deny-net',
          '--deny-env',
          '--deny-run',
          '--deny-ffi',
          '--deny-sys',
          '--deny-import',
          '--no-prompt',
          '--v8-flags=--max-old-space-size=96,--wasm-max-mem-pages=2048',
        ].map((flag) => [flag, args.includes(flag)]),
      )
      assert(Object.values(confinement).every(Boolean), 'missing guest confinement flag')
    }
  }
  return {
    result,
    failure,
    elapsedMs,
    hostUserMs: host.user / 1000,
    hostSystemMs: host.system / 1000,
    guestCpuTicks,
    guestLifetimeMs,
    hostPeakKiB: processPeakKiB,
    guestStdoutBytes: stdoutBytes,
    reads,
    baseBytes: input.size,
    declaredBaseBytes: source.byteLength,
    jsonBytes,
    frameCount,
    stagedBytes,
    maxWrite,
    aborts,
    confinement,
    stdinWireBytes: failure
      ? undefined
      : (legacyWholeFrameBytes ??
        8 + headerBytes.length + input.size + frameCount * 4 + frameBytes),
    legacyConcatBytesByLayout: legacyWholeFrameBytes,
    legacyGuestWholeStdinBeforeJsonBytesByLayout: legacyWholeFrameBytes,
  }
}

process.once('message', async (job) => {
  try {
    process.send(await run(job))
  } catch (error) {
    console.error(job.output, error)
    process.exitCode = 1
  } finally {
    process.disconnect()
  }
})
