/**
 * What the app does once its last window has closed.
 *
 * A quit (⌘Q, the Dock, "Restart & Install") closes every window, and Electron
 * calls the quit off the moment a window's `close` event is cancelled. The
 * shell and detached-window close guards cancel it whenever they have to ask
 * the editor something first (a docs tab always: its dirtiness lives in the
 * renderer), then close the window themselves once the answers are in. By then
 * the quit is gone, and on macOS the app would stay running with no window. So
 * a quit that nothing has vetoed is carried out here, when the windows are.
 */
export interface LastWindowClosed {
  /** a headless export closes its hidden window between documents */
  headless: boolean
  /** `before-quit` ran and no close prompt has vetoed the quit since */
  quitting: boolean
  /** the quick-look tray keeps the app running without a window */
  keepInTray: boolean
  platform: NodeJS.Platform
}

export function afterLastWindowClosed(state: LastWindowClosed): 'quit' | 'stay' {
  if (state.headless) return 'stay'
  if (state.quitting) return 'quit'
  if (state.keepInTray) return 'stay'
  return state.platform === 'darwin' ? 'stay' : 'quit'
}
