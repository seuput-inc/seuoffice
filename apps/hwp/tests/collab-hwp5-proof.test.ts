// @vitest-environment node
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import { deflateRawSync } from 'node:zlib'
import init, { HwpDocument } from '@rhwp/core'
import * as CFB from 'cfb'
import { beforeAll, describe, expect, it } from 'vitest'
import { certifyHwp5Source } from '../src/main/collab-hwp5-proof'
import {
  DOC_INFO,
  openHwp,
  recordsOf,
  rewriteStreams,
  SECTION,
  streamOf,
  writeContainer,
} from '../src/main/hwp5-file'

const blank = readFileSync(new URL('../src/renderer/assets/blank.hwp', import.meta.url))

beforeAll(async () => {
  const wasm = join(dirname(createRequire(import.meta.url).resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
})

function modified(path: string, mutate: (data: Buffer) => Buffer): Buffer {
  const opened = openHwp(blank)!
  const bytes = mutate(streamOf(opened, path)!)
  return rewriteStreams(opened, new Map([[path, bytes]]))!
}

function residue(data: Buffer, tag: number, indexFromEnd: number): Buffer {
  const record = recordsOf(data)!.find((r) => r.tag === tag)!
  data[record.end - indexFromEnd] = 0x5a
  return data
}

describe('HWP5 editable source preservation', () => {
  it('certifies the shipped blank and typed text without modifying the supplied bytes', () => {
    const typed = new HwpDocument(blank)
    try {
      typed.insertText(0, 0, 0, '편집할 문단')
      for (const bytes of [blank, typed.exportHwp()]) {
        const before = Uint8Array.from(bytes)
        certifyHwp5Source(bytes, HwpDocument)
        expect(Uint8Array.from(bytes)).toEqual(before)
      }
    } finally {
      typed.free()
    }
  })

  it('accepts a shortened preview only when the body accounts for the exact missing characters', () => {
    const doc = HwpDocument.createEmpty()
    try {
      doc.createBlankDocument()
      doc.splitParagraph(0, 0, doc.textToLogicalOffset(0, 0, 0))
      doc.insertText(0, 1, 0, '한🙂글')
      const bytes = doc.exportHwp()
      certifyHwp5Source(bytes, HwpDocument)
      const reopened = new HwpDocument(bytes)
      try {
        expect(reopened.getTextRange(0, 1, 0, reopened.getParagraphLength(0, 1))).toBe('한🙂글')
      } finally {
        reopened.free()
      }

      const withPreview = (text: string) => {
        const opened = openHwp(bytes)!
        const preview = CFB.find(opened.cfb, '/PrvText')!
        preview.content = Buffer.from(text, 'utf16le')
        preview.size = preview.content.length
        return writeContainer(opened.cfb)
      }
      certifyHwp5Source(withPreview('한글\r\n'), HwpDocument)
      expect(() => certifyHwp5Source(withPreview('한🧩글\r\n'), HwpDocument)).toThrow(/PrvText/)
    } finally {
      doc.free()
    }
  })

  it('refuses a style suffix hidden by the unchanged DocInfo and per-style caches', () => {
    const bytes = modified(DOC_INFO, (data) => residue(data, 26, 1))
    const snapshot = Buffer.from(bytes)
    const doc = new HwpDocument(bytes)
    try {
      expect(streamOf(openHwp(doc.exportHwp())!, DOC_INFO)).toEqual(
        streamOf(openHwp(bytes)!, DOC_INFO),
      )
    } finally {
      doc.free()
    }
    expect(() => certifyHwp5Source(bytes, HwpDocument)).toThrow(/DocInfo/)
    expect(bytes.equals(snapshot)).toBe(true)
  })

  it('refuses a hidden column attribute in section one even when section zero reconstructs', () => {
    const opened = openHwp(blank)!
    const copy = streamOf(opened, SECTION(0))!
    const column = recordsOf(copy)!.find(
      (r) => r.tag === 71 && copy.toString('ascii', r.body, r.body + 4) === 'dloc',
    )!
    copy.writeUInt16LE(0x1234, column.body + 8)
    CFB.utils.cfb_add(opened.cfb, SECTION(1), deflateRawSync(copy), { unsafe: true })
    const info = streamOf(opened, DOC_INFO)!
    info.writeUInt16LE(2, recordsOf(info)![0]!.body)
    const entry = CFB.find(opened.cfb, DOC_INFO)!
    entry.content = deflateRawSync(info)
    entry.size = entry.content.length
    expect(() => certifyHwp5Source(writeContainer(opened.cfb), HwpDocument)).toThrow(
      /BodyText\/Section1/,
    )
  })

  it('refuses a vendor child under the section definition even if its opaque record list retains it', () => {
    const bytes = modified(SECTION(0), (data) => {
      const def = recordsOf(data)!.find((r) => r.tag === 75)!
      const record = Buffer.alloc(5)
      record.writeUInt32LE((1 << 20) | (2 << 10) | 0x3fe)
      record[4] = 0x5a
      return Buffer.concat([data.subarray(0, def.end), record, data.subarray(def.end)])
    })
    expect(() => certifyHwp5Source(bytes, HwpDocument)).toThrow(/0x3fe/)
  })

  it('refuses a cell list extension that a table field edit would replace', () => {
    const doc = new HwpDocument(blank)
    try {
      expect(JSON.parse(doc.createTable(0, 0, 0, 1, 1)).ok).toBe(true)
      const table = doc.exportHwp()
      certifyHwp5Source(table, HwpDocument)
      const opened = openHwp(table)!
      const data = residue(streamOf(opened, SECTION(0))!, 72, 1)
      const custom = rewriteStreams(opened, new Map([[SECTION(0), data]]))!
      expect(() => certifyHwp5Source(custom, HwpDocument)).toThrow(/0x48/)
    } finally {
      doc.free()
    }
  })

  it('does not use copied tracking metadata as proof of a custom value', () => {
    const bytes = modified(DOC_INFO, (data) => residue(data, 32, 1))
    expect(() => certifyHwp5Source(bytes, HwpDocument)).toThrow(/DocInfo/)
  })

  it('refuses hidden cell paint before opening a document whose structural edits rebuild those cells', () => {
    const opened = openHwp(blank)!
    CFB.utils.cfb_add(opened.cfb, '/RhwpCellBorderPaint', Buffer.from('[]'), { unsafe: true })
    expect(() => certifyHwp5Source(writeContainer(opened.cfb), HwpDocument)).toThrow(
      /RhwpCellBorderPaint/,
    )
  })

  it('bounds inflated stream bytes before WASM sees them', () => {
    const opened = openHwp(blank)!
    const entry = CFB.find(opened.cfb, SECTION(0))!
    entry.content = deflateRawSync(Buffer.alloc(1024 * 1024 + 1))
    entry.size = entry.content.length
    expect(() => certifyHwp5Source(writeContainer(opened.cfb), HwpDocument)).toThrow()
  })
})
