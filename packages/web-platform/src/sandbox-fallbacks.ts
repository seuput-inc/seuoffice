import { webLanguage } from './document-store'

/**
 * The embed iframe is sandboxed without allow-popups by default: `window.open` returns null. A link
 * the browser would not open is shown in an in-page dialog with a copy button instead. A
 * `<a target="_blank">` is sent through `window.open` for the same reason.
 */
const TEXT = {
  ko: {
    linkTitle: '이 링크는 이 화면에서 바로 열 수 없습니다',
    copy: '복사',
    copied: '복사했습니다',
    close: '닫기',
  },
  en: {
    linkTitle: 'This link cannot be opened from this view',
    copy: 'Copy',
    copied: 'Copied',
    close: 'Close',
  },
}

function dialog(title: string, body: string, withCopy: string): void {
  const text = TEXT[webLanguage() === 'ko' ? 'ko' : 'en']
  const overlay = document.createElement('div')
  overlay.setAttribute('role', 'dialog')
  overlay.setAttribute('aria-modal', 'true')
  overlay.setAttribute('aria-label', title)
  overlay.dataset.webSandboxDialog = ''
  overlay.style.cssText =
    'position:fixed;inset:0;z-index:2147483647;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,.4);font:14px system-ui,sans-serif'
  const box = document.createElement('div')
  box.style.cssText =
    'box-sizing:border-box;max-width:min(480px,calc(100vw - 24px));padding:16px;border-radius:8px;background:#fff;color:#222;box-shadow:0 8px 32px rgba(0,0,0,.3)'
  const heading = document.createElement('p')
  heading.style.cssText = 'margin:0 0 8px;font-weight:600'
  heading.textContent = title
  box.append(heading)
  const content = document.createElement('p')
  content.style.cssText = 'margin:0 0 12px;overflow-wrap:anywhere;user-select:text'
  content.textContent = body
  box.append(content)
  const row = document.createElement('div')
  row.style.cssText = 'display:flex;gap:8px;justify-content:flex-end'
  const close = document.createElement('button')
  close.type = 'button'
  close.textContent = text.close
  close.addEventListener('click', () => overlay.remove())
  {
    const copy = document.createElement('button')
    copy.type = 'button'
    copy.textContent = text.copy
    copy.addEventListener('click', () => {
      void navigator.clipboard
        ?.writeText(withCopy)
        .then(() => (copy.textContent = text.copied))
        .catch(() => {})
    })
    row.append(copy)
  }
  row.append(close)
  box.append(row)
  overlay.append(box)
  overlay.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') overlay.remove()
  })
  document.body.append(overlay)
  close.focus()
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
    if (href) dialog(TEXT[webLanguage() === 'ko' ? 'ko' : 'en'].linkTitle, href, href)
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
