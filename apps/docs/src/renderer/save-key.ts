import type { DesktopApi } from '../shared/ipc'

/**
 * Ctrl/Cmd+S saves; with Shift it is Save As. The key is compared without its case, because
 * Shift and Caps Lock both report "S".
 *
 * Save As starts with the host's dialog (`pickSaveAsPath`). A host without one (the web page)
 * keeps Ctrl/Cmd+Shift+S: the page neither takes the key nor starts a save that would fail.
 */
export function handleSaveKey(
  event: KeyboardEvent,
  host: Pick<DesktopApi, 'pickSaveAsPath'>,
  save: (saveAs: boolean) => void,
): void {
  if (!(event.metaKey || event.ctrlKey) || String(event.key).toLowerCase() !== 's') return
  if (event.shiftKey && typeof host.pickSaveAsPath !== 'function') return
  event.preventDefault()
  save(event.shiftKey)
}
