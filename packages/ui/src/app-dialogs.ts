/**
 * The confirm, alert and prompt an app asks a person through. A desktop app keeps the window's own
 * (Electron shows them as before). The web entry swaps in dialogs drawn in the page
 * (`setAppDialogs`): an embed's iframe is sandboxed without allow-modals by default, and there
 * `window.confirm` answers false and `alert` and `prompt` show nothing.
 *
 * Every call answers through a promise, so a call site reads the same on both.
 */
export interface AppDialogs {
  confirm(message: string): Promise<boolean>
  alert(message: string): Promise<void>
  /** the text the person entered, or null when they cancelled */
  prompt(message: string, value?: string): Promise<string | null>
}

const windowDialogs: AppDialogs = {
  confirm: async (message) => window.confirm(message),
  alert: async (message) => window.alert(message),
  prompt: async (message, value) => window.prompt(message, value),
}

let current: AppDialogs = windowDialogs

/**
 * The dialogs the apps use. A frame inside the page (the hwp studio) reaches the same ones as
 * `window.parent.seuofficeDialogs`.
 */
export const appDialogs: AppDialogs = {
  confirm: (message) => current.confirm(message),
  alert: (message) => current.alert(message),
  prompt: (message, value) => current.prompt(message, value),
}

declare global {
  interface Window {
    /** set by the web entry; absent in a desktop app */
    seuofficeDialogs?: AppDialogs
  }
}

/** The web entry installs its in-page dialogs before the renderer starts */
export function setAppDialogs(dialogs: AppDialogs): void {
  current = dialogs
  window.seuofficeDialogs = dialogs
}
