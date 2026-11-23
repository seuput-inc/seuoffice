import assert from 'node:assert/strict'
import { fork } from 'node:child_process'
import { closeSync, fsyncSync, openSync, writeSync } from 'node:fs'
import {
  appendFile,
  copyFile,
  mkdir,
  mkdtemp,
  open,
  readFile,
  stat,
  writeFile,
} from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import {
  change,
  fileHash,
  header,
  makeHistory,
  MiB,
  nextSurveyBatch,
  original,
  paint,
  sha,
} from './stream-proof-fixture.mjs'

const fileLimit = 50 * MiB,
  jsonLimit = 32 * MiB

// The retained observations have 180 evenly spaced samples. Publish only an identified measured interval.
function interval(op, start, end) {
  const meta = op.puts[0].v
  const [route, values] = meta.subject.split(', 180 equally spaced elevation samples (m): ')
  const samples = values.split(' ')
  assert.equal(samples.length, 180)
  return {
    id: op.id,
    puts: [
      {
        b: 'metadata',
        v: {
          title: meta.title,
          subject: `${route}, elevation samples ${start + 1} to ${end} of 180 (m): ${samples.slice(start, end).join(' ')}`,
        },
      },
    ],
  }
}

function structure(dims, subject) {
  const parts = [],
    offsets = [0]
  let size = 0
  const emit = (p) => {
    parts.push(p)
    size += typeof p === 'string' ? Buffer.byteLength(p) : p.width * p.height * 3
  }
  const obj = (...body) => {
    offsets.push(size)
    emit(`${offsets.length - 1} 0 obj\n`)
    for (const part of body) emit(part)
    emit('\nendobj\n')
  }
  emit('%PDF-1.7\n')
  obj('<< /Type /Catalog /Pages 2 0 R >>')
  obj('<< /Type /Pages /Kids [ 3 0 R 6 0 R ] /Count 2 >>')
  for (const dim of dims) {
    const page = offsets.length
    obj(
      `<< /Type /Page /Parent 2 0 R /MediaBox [ 0 0 612 792 ] /Rotate 0 /Resources << /XObject << /Im0 ${page + 2} 0 R >> >> /Contents ${page + 1} 0 R >>`,
    )
    obj(`<< /Length ${paint.length} >>\nstream\n${paint}\nendstream`)
    obj(
      `<< /Type /XObject /Subtype /Image /Width ${dim.width} /Height ${dim.height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length ${dim.width * dim.height * 3} >>\nstream\n`,
      dim,
      '\nendstream',
    )
  }
  const hex = Buffer.from('\uFEFF' + subject, 'utf16le')
    .swap16()
    .toString('hex')
    .toUpperCase()
  obj(`<< /Title (${original.title}) /Author (${original.author}) /Subject <${hex}> >>`)
  const xref = size
  emit(`xref\n0 ${offsets.length}\n0000000000 65535 f \n`)
  for (const at of offsets.slice(1)) emit(`${String(at).padStart(10, '0')} 00000 n \n`)
  emit(
    `trailer\n<< /Size ${offsets.length} /Root 1 0 R /Info 9 0 R >>\nstartxref\n${xref}\n%%EOF\n`,
  )
  return { parts, size }
}

function exactLayout() {
  const reference = nextSurveyBatch({ sections: 60000, version: 0 }).ops[0]
  const probe = { width: 2600, height: 3360 }
  for (let count = 180; count >= 150; count--) {
    const subject = interval(reference, 0, count).puts[0].v.subject
    const syntax = structure([probe, probe], subject).size - 6 * probe.width * probe.height
    const pixels = (fileLimit - syntax) / 3
    if (!Number.isInteger(pixels)) continue
    // Adjacent widths are coprime. Select nearly square physical pixels on both portrait pages.
    for (let w = 2550; w < 2660; w++) {
      const residue = (w + 1 - (pixels % (w + 1))) % (w + 1)
      const west = residue + (w + 1) * Math.round(((w * 792) / 612 - residue) / (w + 1))
      const east = (pixels - w * west) / (w + 1)
      if (Math.abs(west / w - 792 / 612) > 0.08 || Math.abs(east / (w + 1) - 792 / 612) > 0.08)
        continue
      const layout = structure(
        [
          { width: w, height: west },
          { width: w + 1, height: east },
        ],
        subject,
      )
      assert.equal(layout.size, fileLimit)
      return { ...layout, subject }
    }
  }
  assert.fail('no exact two-raster layout')
}

export function exactPdf(dir) {
  const { parts, size, subject } = exactLayout()
  const path = join(dir, 'retained.pdf'),
    fd = openSync(path, 'wx')
  const hash = sha(),
    images = []
  function write(chunk) {
    const b = typeof chunk === 'string' ? Buffer.from(chunk) : chunk
    for (let i = 0; i < b.length;) {
      const n = writeSync(fd, b, i, b.length - i)
      assert(n > 0)
      i += n
    }
    hash.update(b)
  }
  try {
    for (const part of parts) {
      if (typeof part === 'string') {
        write(part)
        continue
      }
      const { width, height } = part
      const image = sha(),
        row = Buffer.allocUnsafe(3 * width)
      for (let r = 0; r < height; r++) {
        const y = r / (height - 1)
        for (let c = 0; c < width; c++) {
          const x = (images.length + c / (width - 1)) / 2
          const ridge = y - 0.45 - 0.24 * Math.sin(x * 9)
          const z = Math.round(
            310 +
              1400 * Math.exp(-22 * ridge * ridge) +
              140 * Math.cos(21 * x + 7 * y) +
              95 * Math.sin(29 * y - 11 * x),
          )
          const t = Math.max(0, Math.min(1, z / 2000)),
            light = z % 100 < 5 ? 0.4 : 1
          row[3 * c] = (25 + 225 * t) * light
          row[3 * c + 1] = (104 + 95 * Math.sin(Math.PI * t)) * light
          row[3 * c + 2] = (154 - 95 * t) * light
        }
        image.update(row)
        write(row)
      }
      images.push({ width, height, byteLength: width * height * 3, sha256: image.digest('hex') })
    }
    fsyncSync(fd)
  } finally {
    closeSync(fd)
  }
  return { path, size, hash: hash.digest('hex'), images, subject }
}

function finishHistory(log, pdf, newHeader, budget) {
  const candidates = nextSurveyBatch(log).ops
  const frame = (ops) => change(log.version + 1, 'Surveyor', ops)
  const headerGrowth =
    JSON.stringify(newHeader).length - JSON.stringify(header(pdf.hash, log.version)).length
  const available = budget - log.jsonBytes - headerGrowth - 1 - JSON.stringify(frame([])).length
  // Index real measured intervals by their encoded length. Keep the final Info shorter than the original.
  const choices = candidates.slice(0, 3).map((op) => {
    const map = new Map()
    for (let start = 0; start < 180; start += 7) {
      for (let end = start + 3; end <= 180; end++) {
        const slice = interval(op, start, end),
          bytes = JSON.stringify(slice).length
        if (!map.has(bytes)) map.set(bytes, slice)
      }
    }
    return map
  })
  function find(prefix, need) {
    const options = choices[prefix.length]
    const last = options.get(need)
    if (last && last.puts[0].v.subject.length + 80 < pdf.subject.length) return [...prefix, last]
    if (prefix.length + 1 < choices.length) {
      for (const [cost, op] of options) {
        if (cost + 1 >= need) continue
        const ops = find([...prefix, op], need - cost - 1)
        if (ops) return ops
      }
    }
  }
  const ops = find([], available)
  assert(ops, 'no exact set of measured cross-sections')
  const line = JSON.stringify(frame(ops))
  assert.equal(log.jsonBytes + headerGrowth + 1 + Buffer.byteLength(line), budget)
  return { line, finalMeta: ops.at(-1).puts[0].v, ops: log.ops + ops.length }
}

async function child(name, job) {
  const p = fork(new URL(name, import.meta.url), { stdio: ['ignore', 'inherit', 'inherit', 'ipc'] })
  return new Promise((ok, fail) => {
    let result
    p.once('message', (message) => {
      result = message
    })
    p.once('error', fail)
    p.once('exit', (code, signal) =>
      result && code === 0 ? ok(result) : fail(new Error(`${name} failed (${signal ?? code})`)),
    )
    p.send(job)
  })
}

export async function freshLog(path, pdf, budget, epoch, withUndo = false) {
  const log = makeHistory(path, pdf, budget - 384)
  const newHeader = { ...header(pdf.hash, log.version + 1), epoch }
  if (withUndo) newHeader.undo = { epoch, v: 1, index: 1 }
  const { line, finalMeta, ops } = finishHistory(log, pdf, newHeader, budget)
  await appendFile(log.path, line + '\n', { flush: true })
  return { path: log.path, newHeader, finalMeta, ops, hash: await fileHash(log.path) }
}

const render = (input, history, h, output, expectedError) =>
  child('./stream-proof-worker.mjs', {
    input,
    history,
    header: h,
    take: h.version,
    output,
    expectedError,
  })
const reopen = (pdf, path, meta, angles) =>
  child('./stream-proof-reopen.mjs', {
    path,
    meta: { ...original, ...meta },
    angles,
    images: pdf.images,
  })
async function exactHead(dir) {
  const pdf = exactPdf(dir)
  const log = await freshLog(dir, pdf, jsonLimit, 'terrain-survey')
  const output = join(dir, 'head.pdf')
  const run = await render(pdf, log.path, log.newHeader, output)
  assert.deepEqual([run.baseBytes, run.jsonBytes], [fileLimit, jsonLimit])
  assert.equal(run.result.compensation, undefined)
  const edited = await reopen(pdf, output, log.finalMeta, [90, 270])
  const retained = await reopen(pdf, pdf.path, { subject: pdf.subject }, [0, 0])
  assert.equal(retained.size, fileLimit)
  assert.equal(retained.sha256, pdf.hash)
  assert.equal(await fileHash(log.path), log.hash)
  assert.notEqual(edited.sha256, retained.sha256)
  return {
    dir,
    versions: log.newHeader.version,
    ops: log.ops,
    finalMeta: log.finalMeta,
    historyHash: log.hash,
    images: pdf.images,
    retained,
    edited,
    run,
  }
}

async function exactUndo(dir, prior) {
  const previous = JSON.parse(await readFile(join(prior, 'evidence.json'), 'utf8'))
  const pdf = {
    path: join(prior, 'retained.pdf'),
    size: fileLimit,
    hash: previous.retained.sha256,
    subject: exactLayout().subject,
    images: previous.images,
  }
  assert.equal(await fileHash(pdf.path), pdf.hash)
  const heights = pdf.subject.split('(m): ')[1].split(' ').map(Number)
  const mean = (heights.reduce((sum, h) => sum + h, 0) / heights.length).toFixed(1)
  const subject = `${pdf.subject}. Profile: minimum ${Math.min(...heights)} m, maximum ${Math.max(...heights)} m, mean ${mean} m.`
  assert(subject.length > pdf.subject.length && subject.length <= 1023)
  const oversizedLog = join(dir, 'add-survey-summary.ndjson')
  await appendFile(
    oversizedLog,
    JSON.stringify(
      change(1, 'Analyst', [{ id: 'measure-profile', puts: [{ b: 'metadata', v: { subject } }] }]),
    ) + '\n',
    { flush: true },
  )
  const oversized = await render(
    pdf,
    oversizedLog,
    { ...header(pdf.hash, 1), epoch: 'survey-summary' },
    join(dir, 'oversized.pdf'),
    'unsupported',
  )
  assert.deepEqual(oversized.reads, [fileLimit])
  assert.equal(oversized.frameCount, 1)
  assert.equal(oversized.guestStdoutBytes, Buffer.byteLength('{"error":"unsupported"}'))
  assert.equal(oversized.stagedBytes, 0)
  const capDir = join(dir, 'exact-undo'),
    roomDir = join(dir, 'reserved-undo')
  await mkdir(capDir)
  await mkdir(roomDir)
  // These histories start independently at version 1. Neither produces a shorter copy of a retained log.
  const cap = await freshLog(capDir, pdf, jsonLimit, 'survey-capacity', true)
  const room = await freshLog(roomDir, pdf, jsonLimit - 1024, 'survey-reserved', true)
  const noUndo = ({ undo: _undo, ...h }) => h
  const proof = await render(pdf, cap.path, noUndo(cap.newHeader), join(capDir, 'control.pdf'))
  assert(proof.jsonBytes < jsonLimit)
  const control = await reopen(pdf, join(capDir, 'control.pdf'), cap.finalMeta, [90, 270])
  const refusal = await render(
    pdf,
    cap.path,
    cap.newHeader,
    join(capDir, 'refused.pdf'),
    'unsupported',
  )
  assert.equal(refusal.jsonBytes, jsonLimit)
  assert.equal(refusal.frameCount, cap.newHeader.version)
  assert.deepEqual(refusal.reads, [fileLimit])
  assert.equal(refusal.guestStdoutBytes, Buffer.byteLength('{"error":"unsupported"}'))
  assert.equal(refusal.stagedBytes, 0)
  const run = await render(pdf, room.path, room.newHeader, join(roomDir, 'undone.pdf'))
  assert.equal(run.jsonBytes, jsonLimit - 1024)
  const comp = run.result.compensation
  assert.equal(comp.length, 1)
  assert.equal(typeof comp[0].id, 'string')
  assert.deepEqual(comp[0].puts, [{ b: 'rotations', k: 0, was: 90, v: 0 }])
  const next = (h) => ({ ...noUndo(h), version: h.version + 1 })
  const append = (h) => JSON.stringify(change(h.version + 1, 'History', comp))
  const replayJsonBytes = (undoBytes, h) =>
    undoBytes +
    JSON.stringify(next(h)).length -
    JSON.stringify(h).length +
    1 +
    Buffer.byteLength(append(h))
  const capReplayBytes = replayJsonBytes(refusal.jsonBytes, cap.newHeader)
  assert(
    capReplayBytes > jsonLimit,
    'even after dropping the undo selector the compensation must not fit',
  )
  const replayLog = join(roomDir, 'compensated.ndjson')
  await copyFile(room.path, replayLog)
  await appendFile(replayLog, append(room.newHeader) + '\n', { flush: true })
  const replay = await render(pdf, replayLog, next(room.newHeader), join(roomDir, 'replay.pdf'))
  assert.equal(replay.jsonBytes, replayJsonBytes(run.jsonBytes, room.newHeader))
  assert(replay.jsonBytes <= jsonLimit)
  const undone = await reopen(pdf, join(roomDir, 'undone.pdf'), room.finalMeta, [0, 270])
  const replayed = await reopen(pdf, join(roomDir, 'replay.pdf'), room.finalMeta, [0, 270])
  assert.equal(undone.sha256, replayed.sha256)
  const snapshot = join(dir, 'fork-base.pdf'),
    head = join(prior, 'head.pdf')
  await copyFile(head, snapshot)
  const fd = await open(snapshot, 'r+')
  try {
    await fd.sync()
  } finally {
    await fd.close()
  }
  assert.notEqual((await stat(head)).ino, (await stat(snapshot)).ino)
  const forkBase = await reopen(pdf, snapshot, previous.finalMeta, [90, 270])
  assert.equal(forkBase.sha256, previous.edited.sha256)
  const forkLog = join(dir, 'fork.ndjson'),
    forkMeta = { title: 'Field review', subject: 'Eastern ridge review' }
  await appendFile(
    forkLog,
    JSON.stringify(
      change(1, 'Reviewer', [{ id: 'field-review', puts: [{ b: 'metadata', v: forkMeta }] }]),
    ) + '\n',
    { flush: true },
  )
  const forkRun = await render(
    { path: snapshot, size: forkBase.size },
    forkLog,
    { ...header(forkBase.sha256, 1), epoch: 'field-branch' },
    join(dir, 'fork-edited.pdf'),
  )
  const forked = await reopen(pdf, join(dir, 'fork-edited.pdf'), forkMeta, [90, 270])
  assert.notEqual(forked.sha256, forkBase.sha256)
  for (const file of [snapshot, head]) assert.equal(await fileHash(file), forkBase.sha256)
  assert.equal(await fileHash(join(prior, 'retained.ndjson')), previous.historyHash)
  assert.equal(await fileHash(pdf.path), pdf.hash)
  for (const log of [room, cap]) assert.equal(await fileHash(log.path), log.hash)
  return {
    dir,
    source: pdf.hash,
    oversized,
    cap,
    room,
    proof,
    control,
    refusal,
    capReplayBytes,
    run,
    undone,
    replay,
    replayed,
    forkBase,
    forkRun,
    forked,
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [out, mode = 'head', prior] = process.argv.slice(2)
  assert(
    out && (mode === 'head' || (mode === 'undo' && prior)),
    'usage: node stream-proof-exact.mjs <scratch-directory> [undo <verified-exact-head-directory>]',
  )
  assert(
    import.meta.resolve('@seuoffice/history-renderer').includes('/node_modules/'),
    'run in packed consumer',
  )
  await mkdir(out, { recursive: true })
  const dir = await mkdtemp(join(resolve(out), 'terrain-proof-exact-'))
  const evidence = mode === 'undo' ? await exactUndo(dir, prior) : await exactHead(dir)
  await writeFile(join(dir, 'evidence.json'), JSON.stringify(evidence, null, 2) + '\n')
  console.log(JSON.stringify(evidence))
}
