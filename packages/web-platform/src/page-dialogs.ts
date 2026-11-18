import { webLanguage } from './document-store'

/**
 * Dialogs drawn in the page instead of the browser's. The embed iframe is sandboxed without
 * allow-modals by default: there `window.confirm` answers false at once and `alert` and `prompt`
 * show nothing, so an app that asks through them silently does nothing. The web
 * entry hands `pageDialogs` to the apps (setAppDialogs in @seuoffice/ui); a desktop app keeps the
 * window's own.
 */
const TEXT = {
  ko: {
    ok: '확인',
    cancel: '취소',
    close: '닫기',
    copy: '복사',
    copied: '복사했습니다',
    linkTitle: '이 링크는 이 화면에서 바로 열 수 없습니다',
  },
  en: {
    ok: 'OK',
    cancel: 'Cancel',
    close: 'Close',
    copy: 'Copy',
    copied: 'Copied',
    linkTitle: 'This link cannot be opened from this view',
  },
}

export type PageText = (typeof TEXT)['en']

export const pageText = (): PageText => TEXT[webLanguage() === 'ko' ? 'ko' : 'en']

interface DialogButton<T> {
  label: string
  value: T
  /** focused first, and what Enter in the text field presses */
  primary?: boolean
}

export interface DialogOptions<T> {
  title?: string
  message: string
  /** a text field with this first value */
  field?: string
  buttons: DialogButton<T>[]
  /** what Escape answers */
  cancel: T
  /** buttons that act without closing the dialog, placed before `buttons` */
  extra?: () => HTMLButtonElement[]
}

/** Draw one dialog over the page; it answers with the button pressed and the field's text */
export function showPageDialog<T>(options: DialogOptions<T>): Promise<{ value: T; text: string }> {
  // not Promise.withResolvers: the apps compile this package's source against ES2022
  let resolve!: (answer: { value: T; text: string }) => void
  const promise = new Promise<{ value: T; text: string }>((settle) => (resolve = settle))
  const returnFocus = document.activeElement
  const overlay = document.createElement('div')
  overlay.setAttribute('role', 'alertdialog')
  overlay.setAttribute('aria-modal', 'true')
  overlay.setAttribute('aria-label', options.title ?? options.message)
  overlay.dataset.webSandboxDialog = ''
  overlay.style.cssText =
    'position:fixed;inset:0;z-index:2147483647;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,.4);font:14px system-ui,sans-serif'
  const box = document.createElement('div')
  box.style.cssText =
    'box-sizing:border-box;max-width:min(480px,calc(100vw - 24px));padding:16px;border-radius:8px;background:#fff;color:#222;box-shadow:0 8px 32px rgba(0,0,0,.3)'
  if (options.title !== undefined) {
    const heading = document.createElement('p')
    heading.style.cssText = 'margin:0 0 8px;font-weight:600'
    heading.textContent = options.title
    box.append(heading)
  }
  const content = document.createElement('p')
  content.style.cssText =
    'margin:0 0 12px;overflow-wrap:anywhere;white-space:pre-wrap;user-select:text'
  content.textContent = options.message
  box.append(content)
  let field: HTMLInputElement | null = null
  if (options.field !== undefined) {
    field = document.createElement('input')
    field.type = 'text'
    field.value = options.field
    field.setAttribute('aria-label', options.message)
    field.style.cssText =
      'box-sizing:border-box;width:100%;margin:0 0 12px;padding:4px 6px;font:inherit'
    box.append(field)
  }
  const close = (value: T): void => {
    overlay.remove()
    if (returnFocus instanceof HTMLElement && returnFocus.isConnected)
      returnFocus.focus({ preventScroll: true })
    resolve({ value, text: field?.value ?? '' })
  }
  const row = document.createElement('div')
  row.style.cssText = 'display:flex;gap:8px;justify-content:flex-end'
  row.append(...(options.extra?.() ?? []))
  let primary: HTMLButtonElement | null = null
  for (const spec of options.buttons) {
    const button = document.createElement('button')
    button.type = 'button'
    button.textContent = spec.label
    button.addEventListener('click', () => close(spec.value))
    if (spec.primary) primary = button
    row.append(button)
  }
  box.append(row)
  overlay.append(box)
  overlay.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      event.preventDefault()
      close(options.cancel)
    } else if (event.key === 'Enter' && event.target === field && primary) {
      event.preventDefault()
      primary.click()
    } else if (event.key === 'Tab') {
      // keep the keyboard inside the dialog
      const stops = [...box.querySelectorAll<HTMLElement>('input, button')]
      const at = stops.indexOf(document.activeElement as HTMLElement)
      const next = (at + (event.shiftKey ? -1 : 1) + stops.length) % stops.length
      event.preventDefault()
      stops[next]?.focus()
    }
    // the app's own shortcuts must not act behind the dialog
    event.stopPropagation()
  })
  document.body.append(overlay)
  ;(field ?? primary ?? row.querySelector('button'))?.focus()
  field?.select()
  return promise
}

/** The in-page dialogs the web entry gives the apps (an AppDialogs of @seuoffice/ui) */
export const pageDialogs = {
  async confirm(message: string): Promise<boolean> {
    const text = pageText()
    const { value } = await showPageDialog({
      message,
      buttons: [
        { label: text.cancel, value: false },
        { label: text.ok, value: true, primary: true },
      ],
      cancel: false,
    })
    return value
  },
  async alert(message: string): Promise<void> {
    await showPageDialog({
      message,
      buttons: [{ label: pageText().ok, value: undefined, primary: true }],
      cancel: undefined,
    })
  },
  async prompt(message: string, value = ''): Promise<string | null> {
    const text = pageText()
    const answer = await showPageDialog({
      message,
      field: value,
      buttons: [
        { label: text.cancel, value: false },
        { label: text.ok, value: true, primary: true },
      ],
      cancel: false,
    })
    return answer.value ? answer.text : null
  },
}
