import { describe, expect, it } from 'vitest'
import { eventArg, EventReferences, outcomeFailure, rejectedAnswer } from './event-refs.ts'
import { caseElectron } from './fakes.ts'
import type { Call } from './types.ts'

const request = (view: number, generation: number) => ({
  view,
  channel: 'editor:recovery-copy-now',
  args: [generation],
  buffers: [],
})

describe('per-side renderer event references', () => {
  it('replays each side’s actual generation while comparing all other event fields', () => {
    const ref = eventArg(0, 'editor:recovery-copy-now', 2)
    const calls: Call[] = [
      { channel: 'flush', kind: 'trigger', args: [], defer: true },
      { channel: 'editor:recovery-copy-now-done', kind: 'send', view: 2, args: [ref, null] },
    ]
    const ts = new EventReferences(calls)
    const rust = new EventReferences(calls)
    const left = ts.capture(0, [request(1, 30), request(2, 31)])
    const right = rust.capture(0, [request(1, 30), request(2, 408)])
    expect(left).toEqual(right)
    expect(ts.resolve(calls[1]!.args)).toEqual([31, null])
    expect(rust.resolve(calls[1]!.args)).toEqual([408, null])
    expect(left[0]).toEqual(request(1, 30))
  })

  it('does not invent a response when the referenced event or its argument is missing', () => {
    const ref = eventArg(0, 'editor:recovery-copy-now', 2)
    const refs = new EventReferences([{ channel: 'answer', args: [ref] }])
    expect(() => refs.resolve(ref)).toThrow('not captured')
    expect(() => refs.capture(0, [request(1, 31)])).toThrow('not found')
  })

  it('can retain two request generations so a stale reply remains stale', () => {
    const old = eventArg(0, 'editor:recovery-copy-now')
    const current = eventArg(1, 'editor:recovery-copy-now')
    const refs = new EventReferences([{ channel: 'answer', args: [old, current] }])
    refs.capture(0, [request(1, 11)])
    refs.capture(1, [request(1, 12)])
    expect(refs.resolve([old, current])).toEqual([11, 12])
  })

  it('rejects matching but incorrect success on both sides', () => {
    const calls: Call[] = [
      { channel: '', args: [], await: 0, expectAnswer: rejectedAnswer('write failed') },
    ]
    const incorrect = [{ answer: { value: { $: 'u' }, buffers: [] } }]
    expect(outcomeFailure(calls, incorrect)).toContain('expected')
    expect(outcomeFailure(calls, [{ answer: rejectedAnswer('write failed') }])).toBeUndefined()
  })

  it('marks a crashed or destroyed view and delivers a once listener only once', async () => {
    const electron = caseElectron('/synthetic', { calls: [], views: [{ view: 1 }, { view: 2 }] })
    const first = electron.contents(1)
    const second = electron.contents(2)
    let closes = 0
    first.once('destroyed', () => {
      closes++
    })
    await first.emit('render-process-gone')
    expect(first.isCrashed()).toBe(true)
    expect(second.isCrashed()).toBe(false)
    await first.emit('destroyed')
    await first.emit('destroyed')
    expect(first.isDestroyed()).toBe(true)
    expect(second.isDestroyed()).toBe(false)
    expect(closes).toBe(1)
    expect(() => first.send('after-destroy')).toThrow()
  })
})
