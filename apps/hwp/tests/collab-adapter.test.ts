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
