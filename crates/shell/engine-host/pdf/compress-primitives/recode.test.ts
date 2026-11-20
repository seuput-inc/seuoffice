import { describe, expect, it } from 'vitest'
import { recodeImage, resizeGood } from './recode'

describe('Chromium good-quality image resize', () => {
  it('preserves a constant image through both convolution passes', () => {
    const source = new Uint8ClampedArray(8 * 6 * 4)
    for (let i = 0; i < source.length; i += 4) source.set([40, 120, 200, 255], i)
    const actual = resizeGood(source, 8, 6, 3, 2)
    expect(Array.from(actual)).toEqual(Array.from({ length: 6 }, () => [40, 120, 200, 255]).flat())
  })

  it('averages a symmetric two-pixel input with fixed-point truncation', () => {
    const source = Uint8ClampedArray.from([0, 100, 255, 255, 200, 100, 0, 255])
    expect(Array.from(resizeGood(source, 2, 1, 1, 1))).toEqual([100, 100, 127, 255])
  })
  it('leaves a non-integer source size unchanged rather than failing the job', async () => {
    await expect(
      recodeImage({
        kind: 'raw',
        data: Uint8Array.of(0, 0, 0, 0, 0, 0),
        width: 1.5,
        height: 1,
        components: 3,
        longSide: 2000,
        quality: 75,
      }),
    ).resolves.toBeNull()
  })
})
