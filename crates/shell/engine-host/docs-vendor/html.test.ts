import { expect, it, vi } from 'vitest'
import type { BrowserDriver } from '../../../../packages/html2docx/src/driver'

const convert = vi.hoisted(() => vi.fn())
vi.mock('../../../../packages/html2docx/src/convert', () => ({ convertHtmlToDocx: convert }))
import { convertHtml } from './html'

it('passes the actual page and natural table width to the converter', async () => {
  const close = vi.fn().mockResolvedValue(undefined)
  const driver = { close } as unknown as BrowserDriver
  const signal = new AbortController().signal
  convert.mockResolvedValueOnce({ docx: new Uint8Array([1, 2]) })
  const result = await convertHtml('https://html-asset.localhost/chunk.html', driver, signal, true)
  expect(convert).toHaveBeenCalledWith({ url: 'https://html-asset.localhost/chunk.html' }, driver, { naturalTableWidth: true, signal })
  expect(result).toEqual({ header: { $: 'b', k: 'u8', i: 0 }, buffers: ['AQI='] })
  expect(close).toHaveBeenCalledOnce()
})

it('uses the ordinary table width behavior for HTML export', async () => {
  const driver = { close: vi.fn().mockResolvedValue(undefined) } as unknown as BrowserDriver
  const signal = new AbortController().signal
  convert.mockResolvedValueOnce({ docx: new Uint8Array([3]) })
  await convertHtml('https://html-asset.localhost/document.html', driver, signal, false)
  expect(convert).toHaveBeenCalledWith({ url: 'https://html-asset.localhost/document.html' }, driver, { naturalTableWidth: false, signal })
})

it('destroys the conversion page on abort even while conversion is pending', async () => {
  const close = vi.fn().mockResolvedValue(undefined)
  const driver = { close } as unknown as BrowserDriver
  const controller = new AbortController()
  const pending = Promise.withResolvers<{ docx: Uint8Array }>()
  convert.mockReturnValueOnce(pending.promise)
  const result = convertHtml('https://html-asset.localhost/chunk.html', driver, controller.signal, true)
  controller.abort()
  expect(close).toHaveBeenCalledOnce()
  pending.reject(new DOMException('cancelled', 'AbortError'))
  await expect(result).rejects.toMatchObject({ name: 'AbortError' })
  expect(close).toHaveBeenCalledOnce()
})
