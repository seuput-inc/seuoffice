import { expect, it, vi } from 'vitest'
import { CefHtmlDriver } from './driver'

it('forwards viewport, serialized evaluation, and screenshot options to its own page', async () => {
  const call = vi.fn(async (operation: string, _input: unknown) => operation === 'html.screenshot' ? 'AQI=' : 7)
  const driver = new CefHtmlDriver(call)
  await driver.setViewport({ width: 794, height: 1123, deviceScaleFactor: 2 })
  expect(await driver.evaluate((value: number) => value + 1, 6)).toBe(7)
  expect(await driver.screenshot({ clip: { x: 1, y: 2, width: 3, height: 4 }, omitBackground: true })).toEqual(new Uint8Array([1, 2]))
  expect(call.mock.calls[0]).toEqual(['html.viewport', { width: 794, height: 1123, deviceScaleFactor: 2 }])
  expect(call.mock.calls[1]?.[0]).toBe('html.evaluate')
  expect(call.mock.calls[2]).toEqual(['html.screenshot', { clip: { x: 1, y: 2, width: 3, height: 4 }, omitBackground: true }])
})
