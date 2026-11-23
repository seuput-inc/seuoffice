import assert from 'node:assert/strict'
import { execFileSync, fork } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { copyFile, mkdir, mkdtemp, open, readFile, stat, writeFile } from 'node:fs/promises'
import { availableParallelism, loadavg, release } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  change,
  epoch,
  fileHash,
  header,
  makeHistory,
  makePdf,
  MiB,
  nextSurveyBatch,
  original,
  turn,
} from './stream-proof-fixture.mjs'

const cases = {
  legacy: [0, 64 * 1024],
  small: [MiB, MiB / 4],
  medium: [8 * MiB, 4 * MiB],
  target: [50 * MiB, 32 * MiB],
  negative: [MiB, MiB / 4],
}
const [mode, outRoot] = process.argv.slice(2)
assert(
  (mode === 'all' || Object.hasOwn(cases, mode)) && outRoot,
  'usage: node stream-proof.mjs <legacy|small|medium|target|negative|all> <scratch-directory>',
)
assert.equal(process.platform, 'linux', 'Linux /proc metrics required')
assert.equal(process.arch, 'arm64')
const renderer = fileURLToPath(import.meta.resolve('@seuoffice/history-renderer'))
assert(
  renderer.includes('/node_modules/'),
  'run this from the packed consumer, not a workspace symlink',
)
const pkg = JSON.parse(await readFile(join(dirname(renderer), '../package.json')))
const tickHz = Number(execFileSync('getconf', ['CLK_TCK'], { encoding: 'ascii' }))
assert(tickHz > 0)
await mkdir(outRoot, { recursive: true })
const root = await mkdtemp(join(resolve(outRoot), 'terrain-proof-'))
console.log(
  JSON.stringify({
    directory: root,
    renderer: `${pkg.name}@${pkg.version}`,
    node: process.version,
    kernel: release(),
    glibc: process.report.getReport().header.glibcVersionRuntime,
    arch: process.arch,
    cpus: availableParallelism(),
    loadAverage: loadavg(),
    tickHz,
    limits: {
      baseBytes: 50 * MiB,
      outputBytes: 50 * MiB,
      historyJsonBytes: 32 * MiB,
      guestStdoutBytes: 3 * MiB,
      guestTimeoutMs: 15000,
      oldSpaceMiB: 96,
      wasmMiB: 128,
    },
    measurement:
      'One fresh Node host and Deno guest per render. Wall/host CPU cover public call through file+directory fsync and rename; includes streamed reads, writes and sandbox startup. Excludes fixture generation, loading the host module, preparing legacy arrays, PDF reopen and monitoring. Guest CPU sums wait-accounted user+system ticks. Host peak is process-lifetime kernel maxRSS. Guest peak is highest kernel VmHWM observed during polling; combined peak sums concurrent VmRSS samples, not independent high-water marks. Sampled peaks are lower bounds. RSS includes native/runtime memory; V8 and WASM limits are not RSS limits. No throughput comparison from a single run.',
  }),
)

function proc(path) {
  try {
    return readFileSync(path, 'utf8')
  } catch (err) {
    if (err.code === 'ENOENT' || err.code === 'ESRCH') return ''
    throw err
  }
}
const kib = (status, key) => Number(new RegExp(`^${key}:\\s+(\\d+) kB`, 'm').exec(status)?.[1] ?? 0)

async function child(module, job, measure = false) {
  const p = fork(new URL(module, import.meta.url), {
    stdio: ['ignore', 'inherit', 'inherit', 'ipc'],
  })
  let reply,
    samples = 0,
    guestSamples = 0,
    guestPeakKiB = 0,
    combinedPeakKiB = 0
  const sample = () => {
    const host = proc(`/proc/${p.pid}/status`)
    if (!host) return
    let sum = kib(host, 'VmRSS')
    for (const pid of proc(`/proc/${p.pid}/task/${p.pid}/children`)
      .trim()
      .split(/\s+/)
      .filter(Boolean)) {
      const status = proc(`/proc/${pid}/status`)
      if (!/^Name:\s+deno$/m.test(status)) continue
      guestSamples++
      guestPeakKiB = Math.max(guestPeakKiB, kib(status, 'VmHWM'))
      sum += kib(status, 'VmRSS')
    }
    samples++
    combinedPeakKiB = Math.max(combinedPeakKiB, sum)
  }
  const pollMs = 4
  const timer = measure && setInterval(sample, pollMs)
  try {
    await new Promise((ok, fail) => {
      p.once('message', (m) => {
        reply = m
      })
      p.once('error', fail)
      p.once('exit', (code, signal) =>
        code === 0 && reply
          ? ok()
          : fail(new Error(`${module} failed (${signal ?? code}) for ${job.output ?? job.path}`)),
      )
      p.send(job)
    })
  } finally {
    clearInterval(timer)
  }
  if (measure) {
    if (!reply.failure)
      assert(guestPeakKiB > 0 && reply.guestCpuTicks > 0, 'did not observe the sandbox')
    Object.assign(reply, {
      pollMs,
      samples,
      guestSamples,
      guestPeakKiB,
      combinedPeakKiB,
      guestCpuMs: (reply.guestCpuTicks * 1000) / tickHz,
    })
  }
  return reply
}

async function appendDurable(path, line) {
  const fd = await open(path, 'a')
  try {
    await fd.writeFile(JSON.stringify(line) + '\n')
    await fd.sync()
  } finally {
    await fd.close()
  }
}

for (const scale of mode === 'all' ? Object.keys(cases) : [mode]) {
  const dir = join(root, scale)
  await mkdir(dir)
  const [pdfLimit, historyLimit] = cases[scale]
  const pdf = makePdf(dir, pdfLimit)
  const log = makeHistory(dir, pdf, historyLimit)
  const retainedHash = await fileHash(log.path)
  const head = header(pdf.hash, log.version)
  const undo = { epoch, v: 1, index: 1 }
  const details = {
    scale,
    baseBytes: pdf.size,
    baseHash: pdf.hash,
    images: pdf.images,
    historyBytes: log.jsonBytes,
    historySha256: retainedHash,
    versions: log.version,
    ops: log.ops,
    transects: log.sections,
  }
  details.baseBytesBelowTarget = pdfLimit ? pdfLimit - pdf.size : null
  details.historyBytesBelowTarget = historyLimit - log.jsonBytes
  details.headroom =
    'Two whole RGB rasters fit after reserving 8 KiB for the PDF structures and edited Info. History stops at the last whole operation after reserving 512 bytes for an undo selector or appended compensation.'
  console.log(JSON.stringify({ fixture: details }))
  const runs = {},
    opened = {}
  async function render(label, override = {}) {
    const job = {
      input: pdf,
      history: log.path,
      header: head,
      take: log.version,
      output: join(dir, label + '.pdf'),
      legacy: scale === 'legacy',
      ...override,
    }
    const m = await child('./stream-proof-worker.mjs', job, true)
    runs[label] = m
    console.log(JSON.stringify({ scale, label, ...m }))
    if (!job.expectedError) {
      if (job.header.undo) assert(m.result.compensation?.length)
      else assert.equal(m.result.compensation, undefined)
      if (job.header === head) assert.equal(m.jsonBytes, log.jsonBytes)
      assert.equal(m.stagedBytes, (await stat(job.output)).size)
    }
    return { path: job.output, size: m.stagedBytes, compensation: m.result?.compensation }
  }
  async function inspect(label, file, metadata, angles) {
    const m = await child('./stream-proof-reopen.mjs', {
      path: file.path,
      meta: { ...original, ...metadata },
      angles,
      images: pdf.images,
    })
    opened[label] = m
    return m.sha256
  }
  if (scale === 'negative') {
    const conflictPath = join(dir, 'same-target.ndjson')
    await copyFile(log.path, conflictPath)
    await appendDurable(
      conflictPath,
      change(log.version + 1, 'Bob', [turn('later-west', 0, 90, 180)]),
    )
    await render('same-target', {
      history: conflictPath,
      take: log.version + 1,
      header: { ...head, version: log.version + 1, undo },
      expectedError: 'conflict',
    })
    await render('bad-hash', {
      header: { ...head, baseHash: '0'.repeat(64) },
      expectedError: 'invalid_history',
    })
    for (const fault of ['missing-first', 'wrong-size', 'changed'])
      await render(fault, { fault, expectedError: 'invalid_history' })
    await render('declared-cap', {
      fault: 'declared-cap',
      declaredBytes: 50 * MiB + 1,
      expectedError: 'invalid_history',
    })
    // Give the malformed file its true hash, so fingerprint checks alone cannot reject it.
    const path = join(dir, 'wrong-components.pdf')
    await copyFile(pdf.path, path)
    const fd = await open(path, 'r+')
    try {
      const first = Buffer.alloc(1024)
      await fd.read(first, 0, first.length, 0)
      const parts = '/BitsPerComponent 8'
      const at = first.indexOf(parts)
      assert(at >= 0)
      assert.equal((await fd.write(Buffer.from('1'), 0, 1, at + parts.length - 1)).bytesWritten, 1)
    } finally {
      await fd.close()
    }
    await render('malformed-image', {
      input: { ...pdf, path },
      header: { ...head, baseHash: await fileHash(path) },
      expectedError: 'unsupported',
    })
  } else if (scale === 'legacy') {
    const result = await render('legacy-head')
    await inspect('legacy-head', result, log.lastMeta, [90, 270])
  } else {
    const prefix = await render('prefix', { take: 1, header: { ...head, version: 1 } })
    const exported = await render('head')
    const snapshot = await render('fork-base')
    const undone = await render('undo', { header: { ...head, undo } })
    const comp = undone.compensation
    assert.equal(comp.length, 1)
    assert.equal(typeof comp[0].id, 'string')
    assert.deepEqual(comp[0].puts, [{ b: 'rotations', k: 0, was: 90, v: 0 }])
    const replayPath = join(dir, 'compensated.ndjson')
    await copyFile(log.path, replayPath)
    await appendDurable(replayPath, change(log.version + 1, 'History', comp))
    const replay = await render('replay', {
      history: replayPath,
      take: log.version + 1,
      header: { ...head, version: log.version + 1 },
    })
    const forkBaseHash = await fileHash(snapshot.path)
    const forkPath = join(dir, 'fork.ndjson')
    const forkMeta = { title: 'Field team fork', subject: 'Review of the eastern ridge' }
    await appendDurable(
      forkPath,
      change(1, 'Reviewer', [{ id: 'review', puts: [{ b: 'metadata', v: forkMeta }] }]),
    )
    const forkEdit = await render('fork-edited', {
      input: snapshot,
      history: forkPath,
      take: 1,
      header: { ...header(forkBaseHash, 1), epoch: 'terrain-field-review' },
    })
    const headHash = await inspect('head', exported, log.lastMeta, [90, 270])
    assert.equal(await inspect('fork-base', snapshot, log.lastMeta, [90, 270]), forkBaseHash)
    assert.equal(headHash, forkBaseHash)
    assert.notEqual((await stat(snapshot.path)).ino, (await stat(exported.path)).ino)
    assert.notEqual(await inspect('fork-edited', forkEdit, forkMeta, [90, 270]), headHash)
    await inspect('prefix', prefix, log.prefixMeta, [90, 0])
    const undoHash = await inspect('undo', undone, log.lastMeta, [0, 270])
    assert.equal(await inspect('replay', replay, log.lastMeta, [0, 270]), undoHash)
    if (scale === 'target') {
      const over = join(dir, 'one-more-batch.ndjson')
      await copyFile(log.path, over)
      await appendDurable(over, nextSurveyBatch(log))
      await render('history-cap', {
        fault: 'history-cap',
        history: over,
        take: log.version + 1,
        header: { ...head, version: log.version + 1 },
        expectedError: 'invalid_history',
      })
    }
  }
  assert.equal(await inspect('original', pdf, {}, [0, 0]), pdf.hash)
  assert.equal(await fileHash(log.path), retainedHash)
  for (const label of Object.keys(runs)) assert(!existsSync(join(dir, label + '.pdf.partial')))
  const measurements = Object.values(runs)
  const totals = {
    successfulRenders: measurements.filter((r) => !r.failure).length,
    expectedRefusals: measurements.filter((r) => r.failure).length,
    renderWallMs: measurements.reduce((n, r) => n + r.elapsedMs, 0),
    hostCpuMs: measurements.reduce((n, r) => n + r.hostUserMs + r.hostSystemMs, 0),
    guestCpuMs: measurements.reduce((n, r) => n + r.guestCpuMs, 0),
    confirmedOutputBytes: measurements.reduce((n, r) => n + (r.failure ? 0 : r.stagedBytes), 0),
  }
  await writeFile(
    join(dir, 'evidence.json'),
    JSON.stringify({ details, runs, opened, totals }, null, 2) + '\n',
  )
  console.log(JSON.stringify({ verified: scale, totals, evidence: join(dir, 'evidence.json') }))
}
