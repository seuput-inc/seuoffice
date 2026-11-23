import { Buffer } from 'node:buffer'
import { renderGuestBytes } from './byte-engine.js'

const { Deno } = globalThis
const input = Buffer.from(await new Response(Deno.stdin.readable).arrayBuffer())
let meta
let bytes = new Uint8Array()
try {
  const end = 4 + input.readUInt32LE(0)
  if (end > input.length || input.length - end > 1024 * 1024) throw new Error('frame')
  const json = new TextDecoder('utf-8', { fatal: true }).decode(input.subarray(end))
  const request = JSON.parse(json)
  const file = await renderGuestBytes(input.subarray(4, end), request)
  meta = { fileName: file.fileName, compensation: file.compensation }
  bytes = file.bytes
} catch (error) {
  meta = {
    error:
      error?.code === 'unsupported' || error?.code === 'conflict' ? error.code : 'invalid_history',
  }
}
const header = Buffer.from(JSON.stringify(meta))
const length = Buffer.alloc(4)
length.writeUInt32LE(header.length)
const writer = Deno.stdout.writable.getWriter()
for (const part of [length, header, bytes]) await writer.write(part)
writer.releaseLock()
