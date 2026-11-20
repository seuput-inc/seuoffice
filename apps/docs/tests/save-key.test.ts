/**
 * Ctrl/Cmd+S and Ctrl/Cmd+Shift+S on the page. Save As needs the host's dialog: the desktop has
 * one, the web page does not, and there the key stays the browser's.
 */
import { beforeAll, describe, expect, it, vi } from 'vitest'
import { handleSaveKey } from '../src/renderer/save-key'
import type { DesktopApi } from '../src/shared/ipc'
import { installBridges } from '../src/web/bridge'

function press(host: Pick<DesktopApi, 'pickSaveAsPath'>, init: KeyboardEventInit) {
  const save = vi.fn<(saveAs: boolean) => void>()
  const onKey = (event: KeyboardEvent): void => handleSaveKey(event, host, save)
  const event = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init })
  window.addEventListener('keydown', onKey)
  document.body.dispatchEvent(event)
  window.removeEventListener('keydown', onKey)
  return { event, save }
}

describe('a desktop host', () => {
  const desktop = { pickSaveAsPath: async () => null }

  it('saves on Ctrl+S, with Caps Lock on too', () => {
    for (const key of ['s', 'S']) {
      const { event, save } = press(desktop, { key, ctrlKey: true })
      expect(save.mock.calls).toEqual([[false]])
      expect(event.defaultPrevented).toBe(true)
    }
  })

  it('runs Save As on Ctrl+Shift+S and Cmd+Shift+S', () => {
    for (const modifier of [{ ctrlKey: true }, { metaKey: true }]) {
      const { event, save } = press(desktop, { key: 'S', shiftKey: true, ...modifier })
      expect(save.mock.calls).toEqual([[true]])
      expect(event.defaultPrevented).toBe(true)
    }
  })

  it('takes no other key', () => {
    const { event, save } = press(desktop, { key: 's' })
    expect(save).not.toHaveBeenCalled()
    expect(event.defaultPrevented).toBe(false)
  })
})

describe('the web page', () => {
  beforeAll(() => installBridges())

  it('saves on Ctrl+S', () => {
    const { event, save } = press(window.desktop, { key: 's', ctrlKey: true })
    expect(save.mock.calls).toEqual([[false]])
    expect(event.defaultPrevented).toBe(true)
  })

  it('leaves Ctrl+Shift+S and Cmd+Shift+S to the browser', () => {
    for (const modifier of [{ ctrlKey: true }, { metaKey: true }]) {
      const { event, save } = press(window.desktop, { key: 'S', shiftKey: true, ...modifier })
      expect(save).not.toHaveBeenCalled()
      expect(event.defaultPrevented).toBe(false)
    }
  })
})
