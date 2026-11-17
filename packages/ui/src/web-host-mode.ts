/**
 * What the web embed host asked of a page. The desktop apps never set either: the mode comes from
 * the page's query, the AI offer from an attribute the web bridge sets before the renderer starts.
 */

/** `?mode=view`: the host opened the page only to read the document */
export function webViewMode(): boolean {
  return new URLSearchParams(globalThis.location?.search ?? '').get('mode') === 'view'
}

const WEB_AI_ATTRIBUTE = 'data-web-ai'

/** The web bridge records whether the embed host offers AI, before the renderer starts */
export function markWebAiOffer(offered: boolean): void {
  document.documentElement.setAttribute(WEB_AI_ATTRIBUTE, offered ? 'on' : 'off')
}

/** The page was embedded by a host that offers no AI: the app shows no AI panel and no AI button */
export function webAiOff(): boolean {
  return document.documentElement.getAttribute(WEB_AI_ATTRIBUTE) === 'off'
}
