import { streamPlan } from './stream-replay'

const { Deno } = globalThis
let result
try {
  result = await streamPlan(Deno.stdin.readable)
} catch (error) {
  result = {
    error: ['unsupported', 'conflict'].includes(error?.code) ? error.code : 'invalid_history',
  }
}
const writer = Deno.stdout.writable.getWriter()
await writer.write(new TextEncoder().encode(JSON.stringify(result)))
writer.releaseLock()
