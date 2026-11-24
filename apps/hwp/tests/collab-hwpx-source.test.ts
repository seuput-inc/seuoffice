// @vitest-environment node
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import JSZip from 'jszip'
import { beforeAll, describe, expect, it } from 'vitest'
import { certifyHwpxSource } from '../src/shared/collab-hwpx-source'

let source: Uint8Array
const section = 'Contents/section0.xml'
const header = 'Contents/header.xml'
const manifest = 'Contents/content.hpf'

beforeAll(async () => {
  const wasm = join(dirname(createRequire(import.meta.url).resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
  const doc = HwpDocument.createEmpty()
  try {
    doc.createBlankDocument()
    doc.insertText(0, 0, 0, '원본 & 띄어쓰기')
    source = doc.exportHwpx()
  } finally {
    doc.free()
  }
})

async function replace(part: string, before: string, after: string) {
  const zip = await JSZip.loadAsync(source)
  const xml = await zip.file(part)!.async('string')
  expect(xml).toContain(before)
  zip.file(part, xml.replace(before, after), { createFolders: false })
  return zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
}

describe('HWPX source preservation before collaboration', () => {
  it('admits an engine-created document without changing its source', async () => {
    const original = source.slice()
    await certifyHwpxSource(source, HwpDocument)
    expect(source).toEqual(original)
  })

  it('compares expanded element names and decoded text, not their XML spelling', async () => {
    const bytes = await replace(
      section,
      '<hp:t>원본 &amp; 띄어쓰기</hp:t>',
      '<x:t xmlns:x="http://www.hancom.co.kr/hwpml/2011/paragraph">원본 &#38; 띄어쓰기</x:t>',
    )
    await expect(certifyHwpxSource(bytes, HwpDocument)).resolves.toBeUndefined()
  })

  it.each([
    [section, '<hp:p ', '<hp:p hp:unmodelled="preserve" '],
    [section, '</hs:sec>', '<hp:t>omitted by the model</hp:t></hs:sec>'],
    [header, '<hh:compatibleDocument ', '<hh:compatibleDocument hh:unmodelled="preserve" '],
    [header, '<hh:paraHead ', '<hh:paraHead unmodelled="preserve" '],
    [manifest, '<opf:title/>', '<opf:title>metadata copied verbatim</opf:title>'],
    [manifest, '<opf:item ', '<opf:item opf:unmodelled="preserve" '],
    ['settings.xml', '<ha:CaretPosition ', '<ha:CaretPosition ha:unmodelled="preserve" '],
  ])('refuses unmodelled content in %s (%s)', async (part, before, after) => {
    const bytes = await replace(part, before, after)
    await expect(certifyHwpxSource(bytes, HwpDocument)).rejects.toThrow(part)
  })

  it('refuses an image asset the document model will not write back', async () => {
    const zip = await JSZip.loadAsync(source)
    zip.file('BinData/orphan.png', Uint8Array.of(1, 2, 3), { createFolders: false })
    const bytes = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
    await expect(certifyHwpxSource(bytes, HwpDocument)).rejects.toThrow('BinData/orphan.png')
  })
})
