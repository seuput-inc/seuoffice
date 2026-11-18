import type { PDFDocumentProxy } from 'pdfjs-dist'
import type { WebThumbnailRequest } from '@seuoffice/ui'

/**
 * The first page of the file the viewer holds, on a canvas of its own: what the file says,
 * without the marks and edits a person has not saved yet. The viewer's own canvases are
 * left alone, so the picture does not depend on zoom, scroll or night reading.
 */
export async function drawFirstPage(
  doc: PDFDocumentProxy,
  { width, maxHeight, signal }: WebThumbnailRequest,
): Promise<HTMLCanvasElement> {
  signal.throwIfAborted()
  const page = await doc.getPage(1)
  const viewport = page.getViewport({ scale: width / page.getViewport({ scale: 1 }).width })
  const canvas = document.createElement('canvas')
  canvas.width = width
  canvas.height = Math.max(1, Math.min(Math.round(viewport.height), maxHeight))
  const task = page.render({ canvas, viewport })
  const stop = (): void => task.cancel()
  signal.addEventListener('abort', stop, { once: true })
  try {
    await task.promise
  } finally {
    signal.removeEventListener('abort', stop)
  }
  return canvas
}
