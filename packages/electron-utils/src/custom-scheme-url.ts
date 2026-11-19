/**
 * The URL a page uses for one of the shell's custom schemes. The app names such a
 * URL by its logical form, `<scheme>://<authority>/<path>?<query>#<hash>`. WKWebView
 * (macOS) and Electron serve that form as it is. WebView2 (Tauri on Windows) serves a
 * custom scheme only as `http://<scheme>.localhost/<authority>/<path>?<query>#<hash>`,
 * the authority becoming the first path segment (crates/shell/src/native/url.rs is the
 * Rust twin of this mapping).
 *
 * The shell's preload says which form the page lives in by setting
 * {@link SCHEME_FORM_GLOBAL}; without it the page is Electron or macOS and every URL stays
 * logical. Replies and events of the shell's IPC are converted by the bridge, so a page only
 * converts the URLs it builds itself, through {@link toPlatformSchemeUrl}.
 */

/** `scheme`: the logical form is served as it is; `localhost-http`: WebView2's form */
export type SchemeForm = 'scheme' | 'localhost-http'

/** The property of the page's global the shell's preload sets before any page script runs */
export const SCHEME_FORM_GLOBAL = '__SEUOFFICE_SCHEME_FORM__'

/** The custom schemes whose URLs a page loads from the shell */
export const CUSTOM_SCHEMES = [
  'md-asset',
  'html-asset',
  'html-preview',
  'epub-asset',
  'seuoffice-docx-media',
  'seuoffice-slide-media',
  'drawio-app',
] as const

/**
 * The schemes the shell hands out in IPC replies and events (the bridge maps these both
 * ways). `md-asset` is built by the page alone, and its authority is empty, so a platform URL
 * could not be told back from a logical one with an authority.
 */
export const SHELL_SCHEMES: readonly string[] = CUSTOM_SCHEMES.filter(
  (scheme) => scheme !== 'md-asset',
)

const LOGICAL = /^([a-z][a-z0-9+.-]*):\/\/([^/?#]*)([^]*)$/
const PLATFORM = /^http:\/\/([a-z][a-z0-9+.-]*)\.localhost(\/[^/?#]*)?([^]*)$/

/** The form a page lives in, from what Tauri's `convertFileSrc('', <scheme>)` answers there. */
export function schemeFormOfProbe(probe: string): SchemeForm {
  return /^https?:\/\//.test(probe) ? 'localhost-http' : 'scheme'
}

/** The form the shell's preload set on this page; `scheme` when it set none. */
export function currentSchemeForm(): SchemeForm {
  const form = (globalThis as Record<string, unknown>)[SCHEME_FORM_GLOBAL]
  return form === 'localhost-http' ? 'localhost-http' : 'scheme'
}

/**
 * `url` in the form the page can load. A URL of another scheme, or one that is already in
 * the platform form, is returned as it is.
 */
export function toPlatformSchemeUrl(
  url: string,
  form: SchemeForm = currentSchemeForm(),
  schemes: readonly string[] = CUSTOM_SCHEMES,
): string {
  if (form === 'scheme') return url
  const match = LOGICAL.exec(url)
  if (!match || !schemes.includes(match[1]!)) return url
  const [, scheme, authority, rest] = match
  return `http://${scheme}.localhost${authority ? `/${authority}` : ''}${rest}`
}

/**
 * The logical URL behind a platform URL of one of `schemes`; any other string is returned as it
 * is. Only a scheme whose logical URLs have an authority may be listed.
 */
export function toLogicalSchemeUrl(
  url: string,
  form: SchemeForm = currentSchemeForm(),
  schemes: readonly string[] = SHELL_SCHEMES,
): string {
  if (form === 'scheme') return url
  const match = PLATFORM.exec(url)
  if (!match || !schemes.includes(match[1]!)) return url
  const [, scheme, firstSegment, rest] = match
  if (!firstSegment) return `${scheme}://${rest}`
  return `${scheme}://${firstSegment.slice(1)}${rest}`
}

/** Whether `url` is a URL of `scheme` in either form */
export function isSchemeUrl(url: string, scheme: string): boolean {
  return url.startsWith(`${scheme}:`) || url.startsWith(`http://${scheme}.localhost/`)
}

/** The CSP source that admits the URLs of `scheme` in the form of the page */
export function schemeCspSource(scheme: string, form: SchemeForm = currentSchemeForm()): string {
  return form === 'localhost-http' ? `http://${scheme}.localhost` : `${scheme}:`
}
