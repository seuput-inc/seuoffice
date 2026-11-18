import { describe, expect, it } from 'vitest'

import { afterLastWindowClosed } from '../src/main/last-window-closed'

const base = { headless: false, quitting: false, keepInTray: false } as const

describe('what the app does when its last window has closed', () => {
  it('a quit nobody vetoed is carried out, on every platform', () => {
    for (const platform of ['darwin', 'win32', 'linux'] as const) {
      expect(afterLastWindowClosed({ ...base, quitting: true, platform })).toBe('quit')
    }
  })

  it('closing the last window alone quits Windows and Linux but not macOS', () => {
    expect(afterLastWindowClosed({ ...base, platform: 'win32' })).toBe('quit')
    expect(afterLastWindowClosed({ ...base, platform: 'linux' })).toBe('quit')
    expect(afterLastWindowClosed({ ...base, platform: 'darwin' })).toBe('stay')
  })

  it('the tray keeps the app running after a plain close, not after a quit', () => {
    expect(afterLastWindowClosed({ ...base, keepInTray: true, platform: 'win32' })).toBe('stay')
    expect(
      afterLastWindowClosed({ ...base, keepInTray: true, quitting: true, platform: 'win32' }),
    ).toBe('quit')
  })

  it('a headless export decides its own end', () => {
    expect(
      afterLastWindowClosed({ ...base, headless: true, quitting: true, platform: 'darwin' }),
    ).toBe('stay')
  })
})
