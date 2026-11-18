import type { WebThumbnailRequest } from '@seuoffice/ui'
import type { AgentBridge } from './ai/agent-bridge'

/**
 * The first page as the print preview shows it, edits included, on a canvas of its own: the
 * engine's SVG of the page, painted as an image. An SVG painted that way reads nothing from
 * outside itself, so its text is set in the fonts of the device, not in the editor's own.
 */
export async function drawFirstPage(
  bridge: Pick<AgentBridge, 'previewPage'>,
  { width, maxHeight, signal }: WebThumbnailRequest,
): Promise<HTMLCanvasElement> {
  const page = await bridge.previewPage(0)
  signal.throwIfAborted()
  const image = new Image()
  // a data URL leaves no object URL behind to revoke
  image.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(page.svg)}`
  await image.decode()
  signal.throwIfAborted()
  const scaledHeight = (page.height * width) / page.width
  const canvas = document.createElement('canvas')
  canvas.width = width
  canvas.height = Math.max(1, Math.min(Math.round(scaledHeight), maxHeight))
  const context = canvas.getContext('2d')
  if (!context) throw new Error('this page cannot draw')
  context.drawImage(image, 0, 0, width, scaledHeight)
  return canvas
}
