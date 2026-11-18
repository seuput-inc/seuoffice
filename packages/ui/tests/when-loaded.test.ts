import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { whenLoaded } from '../src/web-host-mode'

beforeEach(() => {
  vi.useFakeTimers()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('waiting for what the app has not loaded yet', () => {
  it('gives the value at once when it is already there, zero and empty text included', async () => {
    const signal = new AbortController().signal
    await expect(whenLoaded(() => 0, signal)).resolves.toBe(0)
    await expect(whenLoaded(() => '', signal)).resolves.toBe('')
  })

  it('gives the value once it appears', async () => {
    let page: { number: number } | null = null
    const waiting = whenLoaded(() => page, new AbortController().signal, 50)
    let got: unknown = 'nothing yet'
    void waiting.then((value) => (got = value))
    await vi.advanceTimersByTimeAsync(120)
    expect(got).toBe('nothing yet')
    page = { number: 1 }
    await vi.advanceTimersByTimeAsync(50)
    expect(got).toEqual({ number: 1 })
  })

  it('rejects when the host stopped waiting, and asks no more after that', async () => {
    const controller = new AbortController()
    const read = vi.fn(() => undefined)
    const waiting = whenLoaded(read, controller.signal, 50)
    const refused = expect(waiting).rejects.toMatchObject({ name: 'AbortError' })
    await vi.advanceTimersByTimeAsync(60)
    controller.abort()
    await vi.advanceTimersByTimeAsync(50)
    await refused
    const asked = read.mock.calls.length
    await vi.advanceTimersByTimeAsync(500)
    expect(read.mock.calls.length).toBe(asked)
  })

  it('rejects without asking when the host had already stopped waiting', async () => {
    const read = vi.fn(() => 1)
    await expect(whenLoaded(read, AbortSignal.abort())).rejects.toMatchObject({
      name: 'AbortError',
    })
    expect(read).not.toHaveBeenCalled()
  })
})
