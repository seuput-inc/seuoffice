import type { HwpApi } from '../shared/ipc'

/**
 * Ctrl+S and Ctrl+P (Cmd on a Mac) pressed in one of `windows` ask `host` for the file command.
 * The editor's frame swallows both keys before the application menu can see them. The Electron
 * main process takes them ahead of the frame; the shell has no such hook, so the page asks for
 * the same file commands the main process would send.
 *
 * The web page (`noSaveAs`) is left alone: its bridge takes Ctrl+S itself where the host stores
 * saves (web/save-key.ts) and answers no print command, so both keys stay the browser's.
 *
 * `suspended` is read at each key. Returns the function that stops listening.
 */
export function watchFileCommandKeys(
  host: Pick<HwpApi, 'noSaveAs' | 'requestFileCommand'>,
  windows: ReadonlyArray<Window | null>,
  suspended: () => boolean,
): () => void {
  if (host.noSaveAs) return () => {}
  const mac = /Mac/.test(navigator.platform)
  const onKey = (event: KeyboardEvent): void => {
    if (suspended() || event.isComposing || event.altKey || event.shiftKey) return
    if (!(mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey)) return
    const command = event.code === 'KeyS' ? 'save' : event.code === 'KeyP' ? 'print' : null
    if (!command) return
    event.preventDefault()
    event.stopPropagation()
    host.requestFileCommand(command)
  }
  const each = (act: (target: Window) => void): void => {
    for (const target of windows) {
      try {
        if (target) act(target)
      } catch {
        /* a frame that went away with its document */
      }
    }
  }
  each((target) => target.addEventListener('keydown', onKey, true))
  return () => each((target) => target.removeEventListener('keydown', onKey, true))
}
