import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { JSDOM } from 'jsdom'
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { mountHwpRecovery } from '../src/renderer/collab-recovery'
import { en } from '../src/renderer/i18n/app/en'

beforeAll(async () => {
  const require = createRequire(import.meta.url)
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
})

const cleanup: (() => void)[] = []
afterEach(() => {
  while (cleanup.length) cleanup.pop()!()
  vi.unstubAllGlobals()
})

function editor(clipboard: Partial<Clipboard>) {
  vi.stubGlobal('navigator', { clipboard })
  const page = new JSDOM('<main></main>').window
  const host = page.document.querySelector('main')!
  const notices = mountHwpRecovery(host)
  const doc = HwpDocument.createEmpty()
  doc.createBlankDocument()
  cleanup.push(() => {
    notices.destroy()
    page.close()
    doc.free()
  })
  return { host, notices, doc }
}

function button(item: Element, name: string) {
  const action = Array.from(item.querySelectorAll('button')).find(
    (each) => each.textContent === name,
  )
  if (!action) throw new Error(`No button labelled ${name}`)
  return action
}

describe('a local insertion that lost its target', () => {
  it('keeps both fragments and copies only the chosen text, verbatim and only on click', async () => {
    const writeText = vi.fn(async (_text: string) => {})
    const { host, notices } = editor({ writeText })
    const text = '<b>초안😀</b>\n  둘째 줄 '
    notices.show({ kind: 'text', text }, () => false)
    const first = host.querySelector('fieldset')!
    notices.show({ kind: 'text', text: '다음 입력' }, () => false)
    expect(first.textContent).toContain(text)
    expect(first.querySelector('b')).toBeNull()
    expect(writeText).not.toHaveBeenCalled()
    button(first, en.collabCopy).click()
    expect(writeText).toHaveBeenCalledExactlyOnceWith(text)
    await vi.waitFor(() => expect(first.textContent).toContain(en.collabCopied))
    expect(host.querySelectorAll('fieldset')).toHaveLength(2)
    button(first, en.collabDiscard).click()
    expect(host.textContent).toContain('다음 입력')
    expect(host.textContent).not.toContain(text)
  })

  it('keeps the text on denial or refused insertion, then inserts at the newly chosen cursor', async () => {
    const { host, notices, doc } = editor({
      writeText: async () => {
        throw new Error('denied')
      },
    })
    doc.insertText(0, 0, 0, '앞뒤')
    let cursor: number | null = null
    notices.show({ kind: 'text', text: '돌아온 글' }, ({ text }) => {
      if (cursor === null) return false
      doc.insertText(0, 0, cursor, text)
      return true
    })
    button(host, en.collabCopy).click()
    await vi.waitFor(() => expect(host.textContent).toContain(en.collabCopyFailed))
    button(host, en.collabReinsert).click()
    await vi.waitFor(() => expect(host.textContent).toContain(en.collabReinsertFailed))
    expect(host.textContent).toContain('돌아온 글')
    expect(doc.getTextRange(0, 0, 0, 2)).toBe('앞뒤')
    cursor = 1
    button(host, en.collabReinsert).click()
    await vi.waitFor(() => expect(host.querySelector('fieldset')).toBeNull())
    expect(doc.getTextRange(0, 0, 0, 7)).toBe('앞돌아온 글뒤')
  })

  it.each([true, false])(
    'recovers the original picture when clipboard support is %s',
    async (supported) => {
      const png = Uint8Array.from(
        readFileSync(join(__dirname, '../src/renderer/assets/file-image.png')),
      )
      const blob = new Blob([png], { type: 'image/png' })
      class ClipboardPicture {
        static supports = () => supported
        constructor(private readonly data: Record<string, Blob>) {}
        async getType(type: string) {
          return this.data[type]
        }
      }
      vi.stubGlobal('ClipboardItem', ClipboardPicture)
      let copied: Blob | undefined
      const { host, notices, doc } = editor({
        write: async ([item]) => {
          copied = await item!.getType('image/png')
        },
      })
      let pictureIndex = -1
      notices.show({ kind: 'picture', blob, name: '그림.png' }, async (picture) => {
        const bytes = new Uint8Array(await picture.blob.arrayBuffer())
        const inserted = JSON.parse(
          doc.insertPicture(0, 0, 0, '', bytes, 7200, 5400, 96, 72, 'png', ''),
        )
        pictureIndex = inserted.controlIdx
        return inserted.ok
      })
      const img = host.querySelector('img')!
      expect(img.alt).toBe('그림.png')
      expect(new Uint8Array(await (await fetch(img.src)).arrayBuffer())).toEqual(png)
      expect(copied).toBeUndefined()
      if (supported) {
        button(host, en.collabCopy).click()
        await vi.waitFor(() => expect(host.textContent).toContain(en.collabCopied))
        expect(new Uint8Array(await copied!.arrayBuffer())).toEqual(png)
      } else {
        expect(Array.from(host.querySelectorAll('button'), (b) => b.textContent)).not.toContain(
          en.collabCopy,
        )
      }
      button(host, en.collabReinsert).click()
      await vi.waitFor(() => expect(host.querySelector('fieldset')).toBeNull())
      expect(doc.getControlImageData(0, 0, '[]', pictureIndex)).toEqual(png)
      await expect(fetch(img.src)).rejects.toThrow()
    },
  )
})
