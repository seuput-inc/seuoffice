import type { RenderSlide } from '@seuoffice/pptx-render'
import { whenLoaded, type WebThumbnailRequest } from '@seuoffice/ui'
import { renderSlideToCanvas } from './export-render'

/** The deck as the editor holds it right now */
export interface DeckOnScreen {
  slides: RenderSlide[]
  images: Map<string, HTMLImageElement>
  /** pictures and fills still decoding; negative until the deck's image effect has run */
  pendingImages: number
}

/**
 * The first slide as the editor draws it, edits included, on a canvas of its own. It waits
 * for the deck, its pictures and its fonts: drawn earlier, the image would keep the gaps and
 * the fallback faces. Konva paints on an animation frame, so a page nobody sees never answers.
 */
export async function drawFirstSlide(
  deck: () => DeckOnScreen,
  { width, maxHeight, signal }: WebThumbnailRequest,
): Promise<HTMLCanvasElement> {
  await whenLoaded(() => {
    const { slides, pendingImages } = deck()
    const settled =
      slides.length > 0 && pendingImages === 0 && window.__seuofficeDocFontsSynced !== false
    return settled ? true : null
  }, signal)
  // the pictures that just decoded reach the deck's state with the next render
  await new Promise((resolve) => requestAnimationFrame(resolve))
  signal.throwIfAborted()
  const { slides, images } = deck()
  const first = slides[0]
  if (!first) throw new Error('the deck has no slide')
  return renderSlideToCanvas(first, images, width / first.widthPx, maxHeight)
}
