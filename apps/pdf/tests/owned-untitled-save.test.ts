// @vitest-environment node
import { describe, expect, it, vi } from 'vitest'
import { createUntitledSaver } from '../src/renderer/untitled-save'

const deferred = <T>() => Promise.withResolvers<T>()
describe('request-owned untitled save correlation', () => {
  it('keeps the original unowned API argument shapes', async () => {
    const pick = vi.fn(async () => true)
    const adopt = vi.fn(async () => ({ ok: true as const, path: '/saved.pdf' }))
    const saver = createUntitledSaver({ pickUntitledCopyTarget: pick, adoptUntitledCopy: adopt })
    await expect(saver.save('/working.pdf', 'Report.pdf', async () => true)).resolves.toEqual({
      saved: true,
      path: '/saved.pdf',
    })
    expect(pick).toHaveBeenCalledWith('/working.pdf', 'Report.pdf')
    expect(adopt).toHaveBeenCalledWith('/working.pdf')
  })

  it('never coalesces a different owner into an already-open dialog', async () => {
    const first = deferred<boolean>()
    const pick = vi
      .fn()
      .mockImplementationOnce(() => first.promise)
      .mockResolvedValueOnce(true)
    const adopt = vi.fn(async () => ({ ok: true as const, path: '/saved.pdf' }))
    const saveFirst = vi.fn(async () => true)
    const saveSecond = vi.fn(async () => true)
    const saver = createUntitledSaver({ pickUntitledCopyTarget: pick, adoptUntitledCopy: adopt })
    const one = saver.save('/working.pdf', 'Report.pdf', saveFirst, 'native-one')
    const two = saver.save('/working.pdf', 'Report.pdf', saveSecond, 'native-two')
    await Promise.resolve()
    expect(pick).toHaveBeenCalledTimes(1)
    first.resolve(false)
    await expect(one).resolves.toEqual({ saved: false, why: 'cancelled' })
    await expect(two).resolves.toEqual({ saved: true, path: '/saved.pdf' })
    expect(saveFirst).not.toHaveBeenCalled()
    expect(saveSecond).toHaveBeenCalledTimes(1)
    expect(pick).toHaveBeenNthCalledWith(1, '/working.pdf', 'Report.pdf', 'native-one')
    expect(pick).toHaveBeenNthCalledWith(2, '/working.pdf', 'Report.pdf', 'native-two')
    expect(adopt).toHaveBeenCalledWith('/working.pdf', 'native-two')
  })
})
