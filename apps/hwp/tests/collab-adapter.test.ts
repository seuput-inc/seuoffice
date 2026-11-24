import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { beforeAll, expect, it, vi } from 'vitest'
import { createHwpCollabAdapter, prepareHwpCollab } from '../src/web/collab-adapter'
import type { HwpOp } from '../src/shared/collab-editor-ops'
import type { HwpStudioControls } from '../studio-plugin/collab-input.mjs'

beforeAll(async () => {
  const dir = dirname(createRequire(import.meta.url).resolve('@rhwp/core'))
  await init({ module_or_path: readFileSync(join(dir, 'rhwp_bg.wasm')) })
})

it('refuses a compound change before publishing its deletion and keeps earlier unconfirmed edits', async () => {
  const bytes = readFileSync(new URL('../src/renderer/assets/blank.hwp', import.meta.url))
  const file = await prepareHwpCollab('hwp', bytes)
  const doc = new HwpDocument(file.bytes)
  const controls: HwpStudioControls = {
    withDocument: (fn) => fn(doc),
    replay: (fn) => fn(doc, null),
    setEditable() {},
    bindPresence() {
      throw new Error('No presence mount in this editor')
    },
  }
  const bound = createHwpCollabAdapter({
    file,
    fileName: 'test.hwp',
    controls,
    onLost: vi.fn(),
    notice: vi.fn(),
  })
  const submit = vi.fn((_ops: HwpOp[]) => {})
  try {
    const { adapter, callbacks } = bound
    adapter.listen(submit)
    adapter.setEditable(true)
    const type = (at: number, text: string) =>
      callbacks.onMutation(doc, 'insertText', [0, 0, at, text], () =>
        doc.insertText(0, 0, at, text),
      )
    callbacks.transaction(doc, () => {
      type(0, '내')
      type(1, ' 글')
    })
    expect(submit).toHaveBeenCalledTimes(1)
    expect(submit.mock.calls[0]![0]).toHaveLength(2)
    const before = await adapter.fingerprint()
    const unsupported = vi.fn()
    expect(() =>
      callbacks.transaction(doc, () => {
        callbacks.onMutation(doc, 'deleteText', [0, 0, 0, 3], () => doc.deleteText(0, 0, 0, 3))
        callbacks.onMutation(doc, 'pasteControl', [0, 0, 0], unsupported)
      }),
    ).toThrow()
    expect(unsupported).not.toHaveBeenCalled()
    expect(submit).toHaveBeenCalledTimes(1)
    expect(doc.getTextRange(0, 0, 0, 3)).toBe('내 글')
    expect(await adapter.fingerprint()).toBe(before)
    adapter.confirmed!(submit.mock.calls[0]![0])
    type(3, '!')
    expect(doc.getTextRange(0, 0, 0, 4)).toBe('내 글!')
    expect(submit).toHaveBeenCalledTimes(2)
  } finally {
    bound.dispose()
    doc.free()
    file.dispose()
  }
})

/** PNG bytes whose header passes picture admission, padded to `size`. */
function png(size: number): Uint8Array {
  const bytes = new Uint8Array(size)
  bytes.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52])
  bytes.set([0, 0, 0, 8, 0, 0, 0, 8, 8, 6, 0, 0, 0], 16)
  return bytes
}

function webp(): Uint8Array {
  const bytes = new Uint8Array(64)
  const ascii = (at: number, text: string) =>
    bytes.set(
      [...text].map((c) => c.charCodeAt(0)),
      at,
    )
  ascii(0, 'RIFF')
  new DataView(bytes.buffer).setUint32(4, bytes.length - 8, true)
  ascii(8, 'WEBPVP8X')
  bytes.set([7, 0, 0, 7, 0, 0], 24)
  return bytes
}

it.each([
  ['a picture past the replay budget', png(200 * 1024), 'png', /144 KB/],
  ['a WebP picture in an HWP document', webp(), 'webp', /WebP/],
] as const)(
  'refuses %s before it changes the document or reaches the server',
  async (_case, data, extension, message) => {
    const bytes = readFileSync(new URL('../src/renderer/assets/blank.hwp', import.meta.url))
    const file = await prepareHwpCollab('hwp', bytes)
    const doc = new HwpDocument(file.bytes)
    const controls: HwpStudioControls = {
      withDocument: (fn) => fn(doc),
      replay: (fn) => fn(doc, null),
      setEditable() {},
      bindPresence() {
        throw new Error('No presence mount in this editor')
      },
    }
    const bound = createHwpCollabAdapter({
      file,
      fileName: 'test.hwp',
      controls,
      onLost: vi.fn(),
      notice: vi.fn(),
    })
    const submit = vi.fn()
    const call = vi.fn()
    try {
      const { adapter, callbacks } = bound
      adapter.listen(submit)
      adapter.setEditable(true)
      const args = [0, 0, 0, '', data, 600, 600, 8, 8, extension, 'stamp']
      expect(() =>
        callbacks.transaction(doc, () => callbacks.onMutation(doc, 'insertPicture', args, call)),
      ).toThrow(message)
      expect(call).not.toHaveBeenCalled()
      expect(submit).not.toHaveBeenCalled()
    } finally {
      bound.dispose()
      doc.free()
      file.dispose()
    }
  },
)
