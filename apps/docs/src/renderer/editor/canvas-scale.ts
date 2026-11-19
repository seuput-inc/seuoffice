/**
 * How the page canvas (`.doc-zoom`) is scaled, and the one place that reads and writes it.
 *
 * CSS `zoom` lays the page out again at the zoomed size. WebKit keeps zoomed text at its
 * minimum logical font size (9 px), so under about 67% the text of a page stops shrinking and
 * every paragraph wraps again: a page fitted to a phone is not the page that was paginated.
 * Where the engine does that, a scale under 100% is a transform instead: the page keeps its
 * 100% layout and is only drawn smaller. Text never grows past its size above 100%, and
 * Chromium (the desktop app) never holds it back, so both keep `zoom`.
 *
 * A transform leaves the layout box at its 100% size. The margins of
 * `transformedCanvasMargins` give the box the room of what is shown, as `zoom` did.
 */
import type { CSSProperties } from 'react'

/** the part of the canvas host the scale lives in: its inline `zoom` or `transform` */
export interface ScaleHost {
  style: { zoom: string; transform: string }
}

let zoomHoldsTextLarge: boolean | undefined

/** True where text under a small CSS zoom is drawn larger than the zoom asks */
function zoomKeepsTextLarge(): boolean {
  if (zoomHoldsTextLarge !== undefined) return zoomHoldsTextLarge
  const width = (zoom: string): number => {
    const probe = document.createElement('span')
    probe.style.cssText =
      'position:absolute;visibility:hidden;white-space:nowrap;font:20px/1 monospace'
    probe.style.zoom = zoom
    probe.textContent = 'mmmmmmmmmm'
    document.body.append(probe)
    const measured = probe.getBoundingClientRect().width
    probe.remove()
    return measured
  }
  // 20 px at a quarter is 5 px: an engine with a 9 px floor draws it almost twice as wide
  zoomHoldsTextLarge = width('0.25') > width('1') * 0.25 * 1.2
  return zoomHoldsTextLarge
}

/** True when the canvas at `scale` is drawn through a transform, not laid out again by `zoom` */
export function canvasIsTransformed(scale: number): boolean {
  return scale < 1 && zoomKeepsTextLarge()
}

/** The inline style that shows the canvas at `scale` */
export function canvasScaleStyle(scale: number): CSSProperties {
  if (!canvasIsTransformed(scale)) return { zoom: scale }
  // flow-root keeps the first and last child's margins inside the box that is scaled
  return { transform: `scale(${scale})`, transformOrigin: '0 0', display: 'flow-root' }
}

/**
 * The scale as the host applies it now (its inline style), not the React state a closure
 * captured: a pass that measures the DOM at factor 1 while the host is still scaled lands
 * every split-row fill in scaled px. Client px divided by it are the page's CSS px.
 */
export function canvasScaleOf(host: ScaleHost | null, fallback: number): number {
  if (!host) return fallback
  const transformed = /^scale\(([^)]+)\)$/.exec(host.style.transform)?.[1]
  const scale = parseFloat(transformed ?? (host.style.zoom || '1'))
  return Number.isFinite(scale) && scale > 0 ? scale : 1
}

/** Shows the canvas at 100% until the returned function runs: a pass measures zoom-1 geometry */
export function unscaleCanvas(host: ScaleHost): () => void {
  const { zoom, transform } = host.style
  if (transform) {
    host.style.transform = ''
    return () => {
      host.style.transform = transform
    }
  }
  host.style.zoom = '1'
  return () => {
    host.style.zoom = zoom
  }
}

/**
 * The margins that make a transformed canvas take the room of what is shown: centred in
 * `room` (its parent's content width) until it overflows, and ending where the shown page
 * ends. `width` and `height` are the canvas's layout size, which a transform does not change.
 */
export function transformedCanvasMargins(
  room: number,
  width: number,
  height: number,
  scale: number,
): { left: number; right: number; bottom: number } {
  return {
    left: Math.max(0, (room - width * scale) / 2),
    right: (scale - 1) * width,
    bottom: (scale - 1) * height,
  }
}
