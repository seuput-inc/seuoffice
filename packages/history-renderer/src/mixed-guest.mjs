import { renderGuestBytes } from './byte-engine.js'
import { streamPlan } from './stream-replay'

const { Deno } = globalThis
let result
try {
  result = await streamPlan(Deno.stdin.readable, renderGuestBytes)
} catch (error) {
  result = {
    error: ['unsupported', 'conflict'].includes(error?.code) ? error.code : 'invalid_history',
    detail: error?.name === 'HistoryError' ? String(error.message).slice(0, 256) : undefined,
  }
}
const writer = Deno.stdout.writable.getWriter()
await writer.write(new TextEncoder().encode(JSON.stringify(result)))
writer.releaseLock()
