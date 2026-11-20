import './transport-instance'
import { installSchemeForm } from './scheme-urls'

interface TauriUrls {
  convertFileSrc(path: string, scheme: string): string
}

/** Tauri's own answer for a custom scheme tells which form this webview serves it in */
const internals = (window as unknown as { __TAURI_INTERNALS__: TauriUrls }).__TAURI_INTERNALS__

export const urlMapping = installSchemeForm(
  window,
  internals.convertFileSrc('', 'seuoffice-scheme-probe'),
)
