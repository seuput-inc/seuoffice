import { afterEach, describe, expect, it, vi } from 'vitest'
import { Hang, withDeadline } from './deadline.ts'

afterEach(() => vi.useRealTimers())

describe('withDeadline', () => {
  it('answers what finishes in time and leaves no timer behind', async () => {
    vi.useFakeTimers()
    await expect(withDeadline('a case', 60_000, Promise.resolve(7))).resolves.toBe(7)
    expect(vi.getTimerCount()).toBe(0)
  })

  it('passes on what the work fails with, and leaves no timer behind', async () => {
    vi.useFakeTimers()
    const failure = withDeadline('a case', 60_000, Promise.reject(new Error('boom')))
    await expect(failure).rejects.toThrow('boom')
    expect(vi.getTimerCount()).toBe(0)
  })

  it('fails work that never finishes with a hang that names it', async () => {
    vi.useFakeTimers()
    const never = new Promise<never>(() => {})
    const failure = withDeadline('the rust side of w/doc-picker (batch W) case 1', 60_000, never)
    const caught = failure.then(
      () => undefined,
      (err: unknown) => err,
    )
    await vi.advanceTimersByTimeAsync(59_999)
    expect(vi.getTimerCount()).toBe(1)
    await vi.advanceTimersByTimeAsync(1)
    const hang = await caught
    expect(hang).toBeInstanceOf(Hang)
    expect((hang as Hang).message).toBe(
      'the rust side of w/doc-picker (batch W) case 1 did not finish in 60 s',
    )
    expect(vi.getTimerCount()).toBe(0)
  })

  it('does not report a hang for work that finishes after its bound was not reached', async () => {
    vi.useFakeTimers()
    let finish!: (value: string) => void
    const work = new Promise<string>((resolve) => (finish = resolve))
    const answered = withDeadline('a case', 1_000, work)
    await vi.advanceTimersByTimeAsync(999)
    finish('late but in time')
    await expect(answered).resolves.toBe('late but in time')
  })
})
