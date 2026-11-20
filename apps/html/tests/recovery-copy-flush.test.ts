import { afterEach, expect, it, vi } from 'vitest'
import { startRecoveryCopies } from '../src/renderer/recovery-copy'

afterEach(() => vi.useRealTimers())

it('queues a fresh tokened flush behind an in-flight periodic copy', async () => {
  vi.useFakeTimers()
  const gate = Promise.withResolvers<void>()
  let flush!: (token?: string) => void
  let text = 'first'
  const writes: Array<[string | null, string | undefined]> = []
  const stop = startRecoveryCopies({
    intervalMs: 10,
    read: () => text,
    write: async (value, token) => {
      writes.push([value, token])
      if (writes.length === 1) await gate.promise
    },
    onFlushRequest: (handler) => {
      flush = handler
      return () => {}
    },
  })
  vi.advanceTimersByTime(10)
  await Promise.resolve()
  text = 'latest'
  flush('request-1')
  expect(writes).toEqual([['first', undefined]])
  gate.resolve()
  for (let i = 0; i < 10; i += 1) await Promise.resolve()
  expect(writes).toEqual([
    ['first', undefined],
    ['latest', 'request-1'],
  ])
  stop()
})

it('reports serialization failure against the matching flush', async () => {
  let flush!: (token?: string) => void
  const write = vi.fn(async (_text: string | null, _token?: string, _error?: string) => {})
  const stop = startRecoveryCopies({
    read: () => {
      throw new Error('serialize failed')
    },
    write,
    onFlushRequest: (handler) => {
      flush = handler
      return () => {}
    },
  })
  flush('request-2')
  await vi.waitFor(() =>
    expect(write).toHaveBeenCalledWith(null, 'request-2', 'Error: serialize failed'),
  )
  stop()
})
