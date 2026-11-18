/**
 * The confirm, alert and prompt the studio asks a person through.
 *
 * The web build frames the studio in an iframe that its host may sandbox without allow-modals;
 * there the window's own `confirm` answers false at once and `alert` and `prompt` show nothing.
 * The hwp page that holds the studio therefore sets `window.seuofficeDialogs` (in-page dialogs,
 * AppDialogs of @seuoffice/ui) and the studio asks those. The desktop app sets nothing, so there
 * the window's own run as before; only the answer arrives through a promise.
 */

/** Module specifier the studio-dialogs-* patches import; rhwpPatches resolves it to this file. */
export const DIALOGS_MODULE = '@ieum/dialogs'

const windowDialogs = {
  confirm: async (message) => globalThis.window.confirm(message),
  alert: async (message) => globalThis.window.alert(message),
  prompt: async (message, value) => globalThis.window.prompt(message, value),
}

/** The dialogs of the page holding this window, else this window's own. */
export function studioDialogs() {
  try {
    return globalThis.window.parent?.seuofficeDialogs ?? windowDialogs
  } catch {
    // a parent on another origin has none
    return windowDialogs
  }
}
