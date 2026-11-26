/**
 * What the web embed host asked of a page. The desktop apps never set any of it: the mode comes
 * from the page's query, the rest from attributes the web entry sets before the renderer starts.
 */

/** `?mode=view`: the host opened the page only to read the document */
export function webViewMode(): boolean {
  return new URLSearchParams(globalThis.location?.search ?? '').get('mode') === 'view'
}

const WEB_HOST_ATTRIBUTE = 'data-web-host'
const WEB_AI_ATTRIBUTE = 'data-web-ai'
const WEB_AUTOSAVE_ATTRIBUTE = 'data-web-autosave'
const WEB_SHARED_ATTRIBUTE = 'data-web-shared'

/**
 * The web entry records what the embed host offers, before the renderer starts. `shared`: the
 * page joins a co-editing session or replays a history, so others' edits reach its document.
 */
export function markWebHost(offers: { ai: boolean; autoSave: boolean; shared?: boolean }): void {
  const root = document.documentElement
  root.setAttribute(WEB_HOST_ATTRIBUTE, 'on')
  root.setAttribute(WEB_AI_ATTRIBUTE, offers.ai ? 'on' : 'off')
  root.setAttribute(WEB_AUTOSAVE_ATTRIBUTE, offers.autoSave ? 'on' : 'off')
  if (offers.shared) root.setAttribute(WEB_SHARED_ATTRIBUTE, 'on')
}

/** A web page that only shows its document: view mode, and nobody else's edits arrive */
export function webShowOnly(): boolean {
  return (
    webEmbedded() &&
    webViewMode() &&
    document.documentElement.getAttribute(WEB_SHARED_ATTRIBUTE) !== 'on'
  )
}

/** The page was embedded by a host that offers no AI: the app shows no AI panel and no AI button */
export function webAiOff(): boolean {
  return document.documentElement.getAttribute(WEB_AI_ATTRIBUTE) === 'off'
}

/** The page runs in a web embed, not in a desktop app */
export function webEmbedded(): boolean {
  return document.documentElement.hasAttribute(WEB_HOST_ATTRIBUTE)
}

/**
 * Whether the app's autosave exists at all: always in a desktop app, and in a web embed only
 * when the host asked for it. Without it the app draws no autosave toggle and runs no timer.
 */
export function autoSaveOffered(): boolean {
  return !webEmbedded() || document.documentElement.getAttribute(WEB_AUTOSAVE_ATTRIBUTE) === 'on'
}

/**
 * What a web embed host's `thumbnail` asks of the app: its first page on a canvas `width`
 * pixels wide, the top `maxHeight` pixels at most. `signal` aborts once the host stopped
 * waiting. The page's web bridge puts the canvas on white and encodes it.
 */
export interface WebThumbnailRequest {
  width: number
  maxHeight: number
  signal: AbortSignal
}

export type WebThumbnailSource = (request: WebThumbnailRequest) => Promise<HTMLCanvasElement>

/**
 * What `read` returns once that is neither null nor undefined, asked again every `pollMs`.
 * Rejects when `signal` aborts first. A host may ask for the first page as soon as the
 * document's bytes were read, before the app has anything to draw.
 */
export async function whenLoaded<T>(
  read: () => T | null | undefined,
  signal: AbortSignal,
  pollMs = 100,
): Promise<T> {
  for (;;) {
    signal.throwIfAborted()
    const value = read()
    if (value !== null && value !== undefined) return value
    await new Promise((resolve) => setTimeout(resolve, pollMs))
  }
}
