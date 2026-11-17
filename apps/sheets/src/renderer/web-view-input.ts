/**
 * The web embed's view mode takes no input that starts an edit: Univer opens its cell editor (and
 * the formula bar's) on a typed character, Enter, F2, a double click, a paste or a drop, and shows
 * what is typed even while the guards (decision 0014) refuse to commit it. These events are stopped
 * before Univer hears them. Moving, selecting, copying, finding and zooming go through, and so do
 * the page's own text fields (the name box, the find panel), where typing is the reading tool.
 */
const EDIT_KEYS = new Set(['Enter', 'F2', 'Delete', 'Backspace'])
const EDIT_CHORDS = new Set(['v', 'x', 'z', 'y', 'd', 'r', 'b', 'i', 'u', 'k', 'e', ';', ':'])

const inTextField = (target: EventTarget | null): boolean =>
  target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement

function startsEdit(event: KeyboardEvent): boolean {
  if (event.isComposing) return true
  if (event.ctrlKey || event.metaKey) return EDIT_CHORDS.has(event.key.toLowerCase())
  return event.key.length === 1 || EDIT_KEYS.has(event.key)
}

export function blockEditingInput(): void {
  const stop = (event: Event): void => {
    event.preventDefault()
    event.stopImmediatePropagation()
  }
  window.addEventListener(
    'keydown',
    (event) => {
      if (!inTextField(event.target) && startsEdit(event)) stop(event)
    },
    true,
  )
  for (const type of ['beforeinput', 'compositionstart', 'paste', 'cut', 'drop', 'dblclick']) {
    window.addEventListener(
      type,
      (event) => {
        if (!inTextField(event.target)) stop(event)
      },
      true,
    )
  }
}
