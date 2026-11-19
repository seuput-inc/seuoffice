import { describe, expect, it } from 'vitest'

import {
  canvasScaleOf,
  transformedCanvasMargins,
  unscaleCanvas,
} from '../src/renderer/editor/canvas-scale'

const host = (zoom: string, transform = '') => ({ style: { zoom, transform } })

describe('the scale a canvas host applies', () => {
  it('is its inline zoom, not the caller fallback', () => {
    expect(canvasScaleOf(host('0.84'), 1)).toBe(0.84)
    expect(canvasScaleOf(host('1'), 0.84)).toBe(1)
  })
  it('is the factor of its transform where the page is scaled that way', () => {
    expect(canvasScaleOf(host('', 'scale(0.43)'), 1)).toBe(0.43)
  })
  it('is 1 for an unset or unusable inline scale', () => {
    expect(canvasScaleOf(host(''), 0.84)).toBe(1)
    expect(canvasScaleOf(host('abc'), 0.84)).toBe(1)
    expect(canvasScaleOf(host('', 'rotate(3deg)'), 0.84)).toBe(1)
  })
  it('is the fallback without a host', () => {
    expect(canvasScaleOf(null, 0.84)).toBe(0.84)
  })
})

describe('a canvas shown at 100% for a measuring pass', () => {
  it.each([
    ['zoomed', host('0.84')],
    ['transformed', host('', 'scale(0.43)')],
  ])('reads 1 while a %s canvas is measured, and its own scale again after', (_, canvas) => {
    const before = canvasScaleOf(canvas, 1)
    const restore = unscaleCanvas(canvas)
    expect(canvasScaleOf(canvas, before)).toBe(1)
    restore()
    expect(canvasScaleOf(canvas, 1)).toBe(before)
  })
})

describe('the room a transformed canvas takes', () => {
  // an A4 page of two sheets, 794 x 2256 px, at 43%: 341.42 x 970.08 shown
  it('is centred in a pane wider than the shown page, and ends where the shown page ends', () => {
    const margins = transformedCanvasMargins(342, 794, 2256, 0.43)
    expect(margins.left).toBeCloseTo((342 - 341.42) / 2)
    // layout width and height less what is shown
    expect(794 + margins.right).toBeCloseTo(341.42)
    expect(2256 + margins.bottom).toBeCloseTo(970.08)
  })
  it('starts at the left edge of a pane narrower than the shown page', () => {
    expect(transformedCanvasMargins(300, 794, 2256, 0.43).left).toBe(0)
  })
})
