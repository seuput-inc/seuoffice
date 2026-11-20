/**
 * The custom-scheme URLs of a page and of the shell's IPC.
 *
 * The shell, its handlers and every value that crosses the IPC name a custom-scheme URL by its
 * logical form (`epub-asset://<token>/<path>`). WebView2 serves such a scheme only as
 * `http://epub-asset.localhost/<token>/<path>`. The bridge settles it at the IPC boundary: a
 * string that is a URL of a scheme the shell hands out becomes the page's form when it
 * arrives, and the logical form when the page sends it. The page learns its own form from
 * {@link SCHEME_FORM_GLOBAL}, for the URLs it builds itself (custom-scheme-url.ts).
 */
import {
  SCHEME_FORM_GLOBAL,
  SHELL_SCHEMES,
  schemeFormOfProbe,
  toLogicalSchemeUrl,
  toPlatformSchemeUrl,
  type SchemeForm,
} from '../../../packages/electron-utils/src/custom-scheme-url'
import type { UrlMapping } from './electron'

/** `undefined` where the page's form is the logical one, so no string is ever looked at there */
export function urlMappingFor(form: SchemeForm): UrlMapping | undefined {
  if (form === 'scheme') return undefined
  return {
    toPage: (text) => toPlatformSchemeUrl(text, form, SHELL_SCHEMES),
    toShell: (text) => toLogicalSchemeUrl(text, form, SHELL_SCHEMES),
  }
}

/**
 * Fixes the page's form on `target` (as a non-writable property, set before any page script
 * runs) and returns the IPC mapping of that form. `probe` is what Tauri's
 * `convertFileSrc('', <scheme>)` answers in this webview.
 */
export function installSchemeForm(target: object, probe: string): UrlMapping | undefined {
  const form = schemeFormOfProbe(probe)
  Object.defineProperty(target, SCHEME_FORM_GLOBAL, {
    value: form,
    enumerable: false,
    writable: false,
    configurable: false,
  })
  return urlMappingFor(form)
}
