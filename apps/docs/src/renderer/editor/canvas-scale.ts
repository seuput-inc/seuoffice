/**
 * How the page canvas (`.doc-zoom`) is scaled, and the one place that reads and writes it:
 * CSS `zoom` on the host's inline style.
 */
import type { CSSProperties } from 'react'

/** the part of the canvas host the scale lives in: its inline `zoom` */
export interface ScaleHost {
  style: { zoom: string }
}

/** The inline style that shows the canvas at `scale` */
export function canvasScaleStyle(scale: number): CSSProperties {
  return { zoom: scale }
}

/**
 * The scale as the host applies it now (its inline style), not the React state a closure
 * captured: a pass that measures the DOM at factor 1 while the host is still scaled lands
 * every split-row fill in scaled px. Client px divided by it are the page's CSS px.
 */
export function canvasScaleOf(host: ScaleHost | null, fallback: number): number {
  if (!host) return fallback
  const scale = parseFloat(host.style.zoom || '1')
  return Number.isFinite(scale) && scale > 0 ? scale : 1
}

/** Shows the canvas at 100% until the returned function runs: a pass measures zoom-1 geometry */
export function unscaleCanvas(host: ScaleHost): () => void {
  const { zoom } = host.style
  host.style.zoom = '1'
  return () => {
    host.style.zoom = zoom
  }
}
