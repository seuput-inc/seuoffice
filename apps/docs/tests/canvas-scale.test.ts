import { describe, expect, it } from 'vitest'

import { canvasScaleOf, unscaleCanvas } from '../src/renderer/editor/canvas-scale'

const host = (zoom: string) => ({ style: { zoom } })

describe('the scale a canvas host applies', () => {
  it('is its inline zoom, not the caller fallback', () => {
    expect(canvasScaleOf(host('0.84'), 1)).toBe(0.84)
    expect(canvasScaleOf(host('1'), 0.84)).toBe(1)
  })
  it('is 1 for an unset or unusable inline scale', () => {
    expect(canvasScaleOf(host(''), 0.84)).toBe(1)
    expect(canvasScaleOf(host('abc'), 0.84)).toBe(1)
  })
  it('is the fallback without a host', () => {
    expect(canvasScaleOf(null, 0.84)).toBe(0.84)
  })
})

describe('a canvas shown at 100% for a measuring pass', () => {
  it('reads 1 while it is measured, and its own scale again after', () => {
    const canvas = host('0.84')
    const restore = unscaleCanvas(canvas)
    expect(canvasScaleOf(canvas, 0.84)).toBe(1)
    restore()
    expect(canvasScaleOf(canvas, 1)).toBe(0.84)
  })
})
