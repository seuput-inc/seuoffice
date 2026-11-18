import { pageText, showPageDialog } from './page-dialogs'

/**
 * The embed iframe is sandboxed without allow-popups by default: `window.open` returns null. A link
 * the browser would not open is shown in an in-page dialog with a copy button instead. A
 * `<a target="_blank">` is sent through `window.open` for the same reason.
 */
function linkDialog(href: string): void {
  const text = pageText()
  void showPageDialog({
    title: text.linkTitle,
    message: href,
    buttons: [{ label: text.close, value: undefined, primary: true }],
    cancel: undefined,
    extra: () => {
      const copy = document.createElement('button')
      copy.type = 'button'
      copy.textContent = text.copy
      copy.addEventListener('click', () => {
        void navigator.clipboard
          ?.writeText(href)
          .then(() => (copy.textContent = text.copied))
          .catch(() => {})
      })
      return [copy]
    },
  })
}

export function installSandboxFallbacks(): void {
  const open = window.open.bind(window)
  window.open = (url?: string | URL, target?: string, features?: string): Window | null => {
    // `noreferrer` would make the browser answer null even when it opened the window
    const opened = open(url, target, features?.replace(/\bnoreferrer\b,?/g, ''))
    if (opened) {
      opened.opener = null
      return opened
    }
    const href = String(url ?? '')
    if (href) linkDialog(href)
    return null
  }
  // a plain `<a target="_blank">` is blocked without a word in the same sandbox: send it through window.open
  document.addEventListener('click', (event) => {
    if (event.defaultPrevented || event.button !== 0) return
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return
    const anchor = (event.target as Element | null)?.closest?.('a[target="_blank"][href]')
    if (!(anchor instanceof HTMLAnchorElement) || !/^https?:$/.test(anchor.protocol)) return
    event.preventDefault()
    window.open(anchor.href, '_blank')
  })
}
