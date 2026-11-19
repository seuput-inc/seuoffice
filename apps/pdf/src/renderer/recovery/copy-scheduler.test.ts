import { expect, it } from 'vitest'
import { createRecoveryCopyScheduler } from './copy-scheduler'

it('reports a failed recovery write rather than acknowledging a flush', async () => {
  const scheduler = createRecoveryCopyScheduler({
    wanted: () => true,
    busy: () => false,
    write: async () => 'failed',
  })
  expect(await scheduler.flushNow()).toBe('failed')
})

it('waits for an in-flight flush before writing later edits', async () => {
  const gate = Promise.withResolvers<'written'>()
  let writes = 0
  const scheduler = createRecoveryCopyScheduler({
    wanted: () => true,
    busy: () => false,
    write: async () => {
      writes += 1
      return writes === 1 ? gate.promise : 'written'
    },
  })
  const first = scheduler.flushNow()
  scheduler.bump()
  const second = scheduler.flushNow()
  expect(writes).toBe(1)
  gate.resolve('written')
  expect(await first).toBe('written')
  expect(await second).toBe('written')
  expect(writes).toBe(2)
})
