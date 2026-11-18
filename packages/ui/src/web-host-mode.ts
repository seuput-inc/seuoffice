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

/** The web entry records what the embed host offers, before the renderer starts */
export function markWebHost(offers: { ai: boolean; autoSave: boolean }): void {
  const root = document.documentElement
  root.setAttribute(WEB_HOST_ATTRIBUTE, 'on')
  root.setAttribute(WEB_AI_ATTRIBUTE, offers.ai ? 'on' : 'off')
  root.setAttribute(WEB_AUTOSAVE_ATTRIBUTE, offers.autoSave ? 'on' : 'off')
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
