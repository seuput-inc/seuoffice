// @vitest-environment jsdom
/**
 * Ctrl+S and Ctrl+P on the page. A desktop host is asked for the file command, because the
 * editor's frame would swallow the key. The web page leaves both keys to the browser, so its
 * print dialog still opens.
 */
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import type { FileCommand } from '../src/shared/ipc'
import { watchFileCommandKeys } from '../src/renderer/file-command-keys'
import { installBridges } from '../src/web/bridge'

let stop = (): void => {}
afterEach(() => stop())

/** The key pressed on the page; `reached` says the page's later listeners saw it */
function press(code: 'KeyS' | 'KeyP'): { event: KeyboardEvent; reached: boolean } {
  const event = new KeyboardEvent('keydown', {
    code,
    ctrlKey: true,
    bubbles: true,
    cancelable: true,
  })
  let reached = false
  const later = (): void => void (reached = true)
  window.addEventListener('keydown', later)
  document.body.dispatchEvent(event)
  window.removeEventListener('keydown', later)
  return { event, reached }
}

describe('a desktop host', () => {
  it.each([
    ['KeyS', 'save'],
    ['KeyP', 'print'],
  ] as const)('is asked for the file command of %s', (code, command) => {
    const requestFileCommand = vi.fn<(command: FileCommand) => void>()
    stop = watchFileCommandKeys({ requestFileCommand }, [window, null], () => false)
    const { event, reached } = press(code)
    expect(requestFileCommand.mock.calls).toEqual([[command]])
    expect(event.defaultPrevented).toBe(true)
    expect(reached).toBe(false)
  })

  it('is not asked while the print preview is open', () => {
    const requestFileCommand = vi.fn<(command: FileCommand) => void>()
    stop = watchFileCommandKeys({ requestFileCommand }, [window], () => true)
    expect(press('KeyP').event.defaultPrevented).toBe(false)
    expect(requestFileCommand).not.toHaveBeenCalled()
  })

  it('is not asked after the watch stopped', () => {
    const requestFileCommand = vi.fn<(command: FileCommand) => void>()
    watchFileCommandKeys({ requestFileCommand }, [window], () => false)()
    expect(press('KeyS').event.defaultPrevented).toBe(false)
    expect(requestFileCommand).not.toHaveBeenCalled()
  })
})

describe('the web page', () => {
  // the page's own bridge, in view mode: nothing else on the page takes Ctrl+S there
  beforeAll(() => installBridges())

  it.each(['KeyS', 'KeyP'] as const)('leaves %s to the browser', (code) => {
    stop = watchFileCommandKeys(window.hwpApi, [window], () => false)
    const { event, reached } = press(code)
    expect(event.defaultPrevented).toBe(false)
    expect(reached).toBe(true)
  })
})
