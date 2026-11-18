/**
 * The first page of the open document as a PNG, for a host's `thumbnail`.
 * The app draws its first page on a canvas; this file sizes that drawing,
 * puts it on paper white, encodes it and bounds the answer, so every app
 * refuses the same way.
 */

/** the image's width in pixels when the host names none */
export const THUMBNAIL_DEFAULT_WIDTH = 480
/** a width outside these two is moved to the nearer one */
export const THUMBNAIL_MIN_WIDTH = 64
export const THUMBNAIL_MAX_WIDTH = 1024
/** a page taller than this many widths is cut off below */
export const THUMBNAIL_MAX_ASPECT = 4
/** an encoded PNG larger than this is refused (`thumbnail_too_large`) */
export const THUMBNAIL_MAX_BYTES = 2 * 1024 * 1024
/** how long the app has to draw and this file to encode (`thumbnail_timeout`) */
export const THUMBNAIL_TIMEOUT_MS = 10_000

export type ThumbnailRefusalCode =
  'thumbnail_unsupported' | 'thumbnail_timeout' | 'thumbnail_too_large' | 'thumbnail_tainted'

export class ThumbnailRefusal extends Error {
  readonly code: ThumbnailRefusalCode

  constructor(code: ThumbnailRefusalCode, message: string) {
    super(message)
    this.name = 'ThumbnailRefusal'
    this.code = code
  }
}

/**
 * The app's part: its first page on a canvas `width` pixels wide, the top
 * `maxHeight` pixels at most. `signal` aborts when the time ran out.
 */
export type ThumbnailSource = (request: {
  width: number
  maxHeight: number
  signal: AbortSignal
}) => Promise<HTMLCanvasElement>

export interface Thumbnail {
  bytes: ArrayBuffer
  width: number
  height: number
}

/** The width an image is drawn at for what the host asked: whole pixels inside the allowed range */
export function thumbnailWidth(requested: unknown): number {
  if (typeof requested !== 'number' || !Number.isFinite(requested)) return THUMBNAIL_DEFAULT_WIDTH
  return Math.min(THUMBNAIL_MAX_WIDTH, Math.max(THUMBNAIL_MIN_WIDTH, Math.round(requested)))
}

function encode(canvas: HTMLCanvasElement): Promise<Blob> {
  return new Promise((resolve, reject) => {
    try {
      canvas.toBlob(
        (blob) =>
          blob
            ? resolve(blob)
            : reject(new ThumbnailRefusal('thumbnail_unsupported', 'the image was not encoded')),
        'image/png',
      )
    } catch (error) {
      // a canvas that drew a picture of another origin gives its pixels to nobody
      reject(
        error instanceof Error && error.name === 'SecurityError'
          ? new ThumbnailRefusal(
              'thumbnail_tainted',
              'the page holds a picture from another origin',
            )
          : error,
      )
    }
  })
}

async function draw(
  source: ThumbnailSource,
  width: number,
  signal: AbortSignal,
): Promise<Thumbnail> {
  const maxHeight = width * THUMBNAIL_MAX_ASPECT
  const page = await source({ width, maxHeight, signal })
  if (page.width < 1 || page.height < 1) {
    throw new ThumbnailRefusal('thumbnail_unsupported', 'the first page has no size')
  }
  const scaledHeight = Math.max(1, Math.round((page.height * width) / page.width))
  const image = document.createElement('canvas')
  image.width = width
  image.height = Math.min(scaledHeight, maxHeight)
  const context = image.getContext('2d')
  if (!context) throw new ThumbnailRefusal('thumbnail_unsupported', 'this page cannot draw')
  // a page without a background of its own is paper, not see-through
  context.fillStyle = '#fff'
  context.fillRect(0, 0, image.width, image.height)
  context.imageSmoothingQuality = 'high'
  context.drawImage(page, 0, 0, width, scaledHeight)
  const blob = await encode(image)
  if (blob.size > THUMBNAIL_MAX_BYTES) {
    throw new ThumbnailRefusal(
      'thumbnail_too_large',
      `the image is ${blob.size} bytes; the limit is ${THUMBNAIL_MAX_BYTES}`,
    )
  }
  return { bytes: await blob.arrayBuffer(), width: image.width, height: image.height }
}

/**
 * The PNG of the first page for the width the host asked. Rejects with a
 * ThumbnailRefusal: no `source` (the app draws none, or nothing is open), the
 * time ran out, the PNG is too large, or the canvas may not be read. Any other
 * rejection is the app's own: what stopped its drawing.
 */
export async function makeThumbnail(
  source: ThumbnailSource | undefined,
  requested: unknown,
): Promise<Thumbnail> {
  if (!source) {
    throw new ThumbnailRefusal('thumbnail_unsupported', 'this page has no first page to draw')
  }
  const waiting = new AbortController()
  const timer = setTimeout(() => waiting.abort(), THUMBNAIL_TIMEOUT_MS)
  const late = new Promise<never>((_, reject) => {
    waiting.signal.addEventListener('abort', () =>
      reject(new ThumbnailRefusal('thumbnail_timeout', 'the image was not ready in time')),
    )
  })
  try {
    return await Promise.race([draw(source, thumbnailWidth(requested), waiting.signal), late])
  } finally {
    clearTimeout(timer)
  }
}
