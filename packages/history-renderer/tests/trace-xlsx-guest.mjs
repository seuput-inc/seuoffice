// Measurement-only entry. The installed sandbox runs it with the production permission and memory flags.
import { createHash } from 'node:crypto'
import { replayHistory } from '@seuoffice/collab/client'
import { checkArchive } from '../src/archive'
import { checkBudget } from '../src/request'
import { openXlsx } from '../src/xlsx'

const { Deno } = globalThis
const ms = {}
let at = performance.now()
function finish(label) {
  const now = performance.now()
  ms[label] = now - at
  at = now
}
let result = new Uint8Array()
let error
let phase = 'stdin'
try {
  const frame = Buffer.from(await new Response(Deno.stdin.readable).arrayBuffer())
  finish(phase)
  const end = 4 + frame.readUInt32LE(0)
  const base = frame.subarray(4, end)
  phase = 'json'
  const request = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(frame.subarray(end)))
  checkBudget(request)
  if (createHash('sha256').update(base).digest('hex') !== request.baseHash) throw Error('hash')
  finish(phase)
  phase = 'zipIn'
  await checkArchive(base)
  finish(phase)
  phase = 'engineOpen'
  const adapter = await openXlsx(base)
  finish(phase)
  phase = 'replayAndSave'
  const file = await replayHistory(
    { ...adapter, listen: () => () => {}, setEditable() {} },
    request.format,
    base,
    request,
  )
  finish(phase)
  phase = 'zipOut'
  await checkArchive(file.bytes)
  finish(phase)
  result = file.bytes
} catch (e) {
  error = { phase, code: e.code, detail: e.message }
  finish(phase)
}
const header = Buffer.from(JSON.stringify({ ms, error }))
const length = Buffer.alloc(4)
length.writeUInt32LE(header.length)
const writer = Deno.stdout.writable.getWriter()
for (const chunk of [length, header, result]) await writer.write(chunk)
writer.releaseLock()
