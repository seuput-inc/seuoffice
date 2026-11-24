// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest'
import { reportError } from '@seuoffice/web-platform'
import type { HwpStudioControls } from '../studio-plugin/collab-input.mjs'
import { openHwpCollabPage } from '../src/web/collab-page'

const ports = vi.hoisted(() => {
  const nextBase = vi.fn<() => Uint8Array | undefined>()
  const save = vi.fn(async () => {})
  const prepare = vi.fn((_format: string, bytes: Uint8Array) => ({ bytes, dispose: vi.fn() }))
  const buffer = vi.fn()
  const binding = vi.fn(() => ({ adapter: {}, callbacks: { onBuffer: buffer }, dispose: vi.fn() }))
  const join = vi.fn((options: { bytes: Uint8Array; adapter(token: symbol): unknown }) => ({
    baseBytes: nextBase() ?? options.bytes,
    attach: async (token: symbol) => {
      options.adapter(token)
    },
    dispose: vi.fn(),
    save,
  }))
  return { nextBase, save, prepare, buffer, binding, join }
})
vi.mock('../src/web/collab-adapter', () => ({
  prepareHwpCollab: ports.prepare,
  createHwpCollabAdapter: ports.binding,
}))
vi.mock('@seuoffice/web-platform', () => ({
  editMode: () => true,
  reportError: vi.fn(),
  webLanguage: () => 'en',
  joinPageCollab: ports.join,
}))

const file = { id: '1', virtualPath: '/web/1/a.hwp', fileName: 'a.hwp', bytes: Uint8Array.of(1) }
afterEach(() => {
  window.dispatchEvent(new Event('pagehide'))
  document.body.replaceChildren()
  vi.clearAllMocks()
})

it('reports source refusal before contacting the epoch or creating an editor', async () => {
  ports.prepare.mockImplementationOnce(() => {
    throw new Error('unverified record')
  })
  await expect(openHwpCollabPage(file)).rejects.toThrow('unverified record')
  expect(ports.prepare).toHaveBeenCalledWith('hwp', file.bytes)
  expect(ports.join).not.toHaveBeenCalled()
  expect(reportError).toHaveBeenCalledExactlyOnceWith(
    'unsupported',
    expect.stringMatching(/original is unchanged.*2 MiB.*unverified record/),
  )
  expect(document.documentElement.hasAttribute('data-studio-collab')).toBe(false)
})

it('does not blame the source file when the session refuses the join', async () => {
  ports.join.mockImplementationOnce(() => {
    throw new Error('offline')
  })
  await expect(openHwpCollabPage(file)).rejects.toThrow('session could not be opened. offline')
  expect(reportError).toHaveBeenCalledTimes(1)
})

it('seeds and opens the certified snapshot, not the caller’s original buffer', async () => {
  const checked = file.bytes.slice()
  ports.prepare.mockReturnValueOnce({ bytes: checked, dispose: vi.fn() })
  expect(await openHwpCollabPage(file)).toBe(checked)
  expect(ports.join.mock.calls[0]![0].bytes).toBe(checked)
  expect(ports.prepare).toHaveBeenCalledTimes(1)
})

it('refuses a late join when the actual epoch base fails admission', async () => {
  const peerBase = Uint8Array.of(2)
  ports.nextBase.mockReturnValueOnce(peerBase)
  ports.prepare
    .mockImplementationOnce((_format, bytes) => ({ bytes, dispose: vi.fn() }))
    .mockImplementationOnce(() => {
      throw new Error('protected epoch base')
    })
  await expect(openHwpCollabPage(file)).rejects.toThrow('protected epoch base')
  expect(ports.prepare.mock.calls).toEqual([
    ['hwp', file.bytes],
    ['hwp', peerBase],
  ])
  expect(ports.binding).not.toHaveBeenCalled()
  expect(window.seuHwpCollabConnect).toBeUndefined()
})

it('accepts the mounted frame once, attaches only after its load, and revokes its callbacks on unmount', async () => {
  const lifetime = new AbortController()
  await openHwpCollabPage(file, lifetime.signal)
  const host = document.createElement('div')
  const recovery = document.createElement('div')
  const other = document.createElement('iframe')
  const frame = document.createElement('iframe')
  document.body.append(other, host, recovery)
  host.append(frame)
  const page = window.seuHwpCollabPage!
  const connect = window.seuHwpCollabConnect!
  const controls: HwpStudioControls = {
    setEditable: vi.fn(),
    withDocument: vi.fn(),
    replay: vi.fn(),
    bindPresence: vi.fn(),
  }
  expect(document.documentElement.hasAttribute('data-studio-collab')).toBe(true)
  expect(() => connect(frame.contentWindow!, controls)).toThrow('Not the active')
  page.mount(host, frame, { recoveryHost: recovery, draftReason: () => null, notice: vi.fn() })
  expect(() => connect(other.contentWindow!, controls)).toThrow('Not the active')
  const hooks = connect(frame.contentWindow!, controls)
  expect(controls.setEditable).toHaveBeenCalledExactlyOnceWith(false)
  expect(() => connect(frame.contentWindow!, controls)).toThrow('Not the active')
  await expect(page.loaded(other)).rejects.toThrow('did not connect')
  expect(ports.binding).not.toHaveBeenCalled()
  await page.loaded(frame)
  hooks.onBuffer(true)
  await expect(page.save(false)).rejects.toThrow('composition')
  expect(ports.save).not.toHaveBeenCalled()
  hooks.onBuffer(false)
  await page.save(false)
  expect(ports.save).toHaveBeenCalledWith({ reason: 'user', skipWhenSaved: true })
  lifetime.abort()
  expect(window.seuHwpCollabConnect).toBeUndefined()
  expect(window.seuHwpCollabPage).toBeUndefined()
  hooks.onBuffer(true)
  expect(ports.buffer.mock.calls).toEqual([[true], [false]])
  await expect(page.save(false)).rejects.toThrow('not ready')
  expect(recovery.childElementCount).toBe(0)
})
