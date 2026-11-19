import { useLayoutEffect } from 'react'

import { canvasIsTransformed, transformedCanvasMargins } from './editor/canvas-scale'

function fitCanvas(host: HTMLElement, scale: number): void {
  const parent = host.parentElement
  if (!parent) return
  const style = getComputedStyle(parent)
  const room =
    parent.clientWidth -
    (parseFloat(style.paddingLeft) || 0) -
    (parseFloat(style.paddingRight) || 0)
  const margins = transformedCanvasMargins(room, host.offsetWidth, host.offsetHeight, scale)
  host.style.marginLeft = `${margins.left}px`
  host.style.marginRight = `${margins.right}px`
  host.style.marginBottom = `${margins.bottom}px`
}

/**
 * While the canvases of the window (the page, and the split pane's copy) are scaled by a
 * transform, keeps each one's margins at the room of what it shows: its scroller then scrolls
 * to the end of the shown page and centres it, as it does under CSS zoom. A layout effect, so
 * the effects after it read the scroller with the scale already in place; an observer follows
 * the page as it grows and the pane as it is resized. `canvases` changes when a canvas mounts
 * or goes.
 */
export function useCanvasExtents(scale: number, canvases: string): void {
  const transformed = canvasIsTransformed(scale)
  useLayoutEffect(() => {
    if (!transformed) return
    const hosts = [...document.querySelectorAll<HTMLElement>('.doc-zoom')]
    const fitAll = (): void => {
      for (const host of hosts) fitCanvas(host, scale)
    }
    fitAll()
    const observer = new ResizeObserver(fitAll)
    for (const host of hosts) {
      observer.observe(host)
      if (host.parentElement) observer.observe(host.parentElement)
    }
    return () => {
      observer.disconnect()
      for (const host of hosts) {
        host.style.marginLeft = ''
        host.style.marginRight = ''
        host.style.marginBottom = ''
      }
    }
  }, [transformed, scale, canvases])
}
