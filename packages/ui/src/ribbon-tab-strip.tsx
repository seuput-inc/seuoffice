import { useLayoutEffect, useRef, type ReactNode } from 'react'

/** Scroll the strip, and only the strip, until `tab` is wholly inside it */
function reveal(strip: HTMLElement, tab: HTMLElement): void {
  const row = strip.getBoundingClientRect()
  const at = tab.getBoundingClientRect()
  if (at.left < row.left) strip.scrollLeft -= row.left - at.left
  else if (at.right > row.right) strip.scrollLeft += at.right - row.right
}

/**
 * The ribbon's tab buttons (ribbon-tab-strip.css). Each name stays on one line; a row narrower
 * than the tabs (a phone-width embed) scrolls sideways by swipe, trackpad or Tab instead of
 * squeezing the names letter by letter. The active tab (`.active`) is scrolled into view whenever
 * `active` changes, so a contextual tab that opens past the edge shows, and so is a tab the
 * keyboard reaches: the browser's own focus scrolling leaves a half-visible tab where it is.
 */
export function RibbonTabStrip({ active, children }: { active: string; children: ReactNode }) {
  const strip = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    const tab = strip.current?.querySelector<HTMLElement>(':scope > .active')
    if (strip.current && tab) reveal(strip.current, tab)
  }, [active])
  return (
    <div
      className="ribbon-tab-strip"
      ref={strip}
      onFocus={(event) => {
        if (event.target.parentElement === event.currentTarget)
          reveal(event.currentTarget, event.target)
      }}
    >
      {children}
    </div>
  )
}
