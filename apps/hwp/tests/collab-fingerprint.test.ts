// @vitest-environment node
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import JSZip from 'jszip'
import { beforeAll, expect, it } from 'vitest'
import { openHwp, recordsOf, rewriteStreams, SECTION, streamOf } from '../src/main/hwp5-file'
import { applyHwpOp, decodeHwpOp, encodeHwpOp } from '../src/shared/collab-editor-ops'
import { rebaseHwpOps } from '../src/shared/collab-transform'
import type { HwpTextSplice } from '../src/shared/collab-text'

beforeAll(async () => {
  const dir = dirname(createRequire(import.meta.url).resolve('@rhwp/core'))
  await init({ module_or_path: readFileSync(join(dir, 'rhwp_bg.wasm')) })
})

function fingerprint(doc: HwpDocument) {
  if (!('getCollabFingerprint' in doc) || typeof doc.getCollabFingerprint !== 'function') {
    throw new Error('Missing authored-model fingerprint')
  }
  return doc.getCollabFingerprint()
}

it.each(['hwp', 'hwpx'] as const)(
  '%s peers with different view settings agree after a concurrent split and insertion',
  (format) => {
    const seed = HwpDocument.createEmpty()
    let alice: HwpDocument | undefined,
      bob: HwpDocument | undefined,
      mirror: HwpDocument | undefined
    try {
      seed.createBlankDocument()
      seed.insertText(0, 0, 0, 'A🙂B')
      const bytes = format === 'hwp' ? seed.exportHwp() : seed.exportHwpx()
      alice = new HwpDocument(bytes)
      bob = new HwpDocument(bytes)
      mirror = new HwpDocument(bytes)
      bob.setDpi(120)
      bob.setShowControlCodes(true)
      bob.setShowParagraphMarks(true)
      expect(fingerprint(bob)).toBe(fingerprint(alice))
      const at = (offset: number) => ({ section: 0, paragraph: 0, path: [], offset })
      const split: HwpTextSplice = { kind: 'splice', from: at(1), to: at(1), text: '\n' }
      const insert: HwpTextSplice = { kind: 'splice', from: at(2), to: at(2), text: '한' }
      applyHwpOp(alice, split)
      applyHwpOp(bob, insert)
      expect(fingerprint(bob)).not.toBe(fingerprint(alice))
      const plan = rebaseHwpOps(mirror, [split], [insert])
      expect(plan.deleted).toEqual([])
      expect(plan.retry).toEqual([])
      if (!('restoreCollabConfirmed' in bob) || typeof bob.restoreCollabConfirmed !== 'function')
        throw new Error('Missing confirmed-model restore')
      bob.restoreCollabConfirmed(mirror)
      for (const op of plan.replay) {
        applyHwpOp(bob, op)
        applyHwpOp(alice, decodeHwpOp(JSON.parse(JSON.stringify(encodeHwpOp(op)))))
      }
      expect(fingerprint(bob)).toBe(fingerprint(alice))
      for (const doc of [alice, bob]) {
        const reopened = new HwpDocument(format === 'hwp' ? doc.exportHwp() : doc.exportHwpx())
        try {
          expect(reopened.getSourceFormat()).toBe(format)
          expect(reopened.getTextRange(0, 0, 0, 99)).toBe('A')
          expect(reopened.getTextRange(0, 1, 0, 99)).toBe('🙂한B')
        } finally {
          reopened.free()
        }
      }
      alice.applyCharFormat(0, 1, 1, 2, '{"bold":true}')
      expect(fingerprint(bob)).not.toBe(fingerprint(alice))
    } finally {
      mirror?.free()
      bob?.free()
      alice?.free()
      seed.free()
    }
  },
)

function digest(bytes: Uint8Array) {
  const doc = new HwpDocument(bytes)
  try {
    return fingerprint(doc)
  } finally {
    doc.free()
  }
}

it('distinguishes the payload and anchor of an opaque HWP paragraph record', () => {
  const seed = HwpDocument.createEmpty()
  try {
    seed.createBlankDocument()
    seed.splitParagraph(0, 0, seed.getParagraphLength(0, 0))
    seed.insertText(0, 1, 0, '보존')
    const source = seed.exportHwp()
    const withRecord = (after: number, byte: number) => {
      const opened = openHwp(source)!
      const path = SECTION(0)
      const data = streamOf(opened, path)!
      const { end } = recordsOf(data)!
        .filter(({ tag }) => tag === after)
        .at(-1)!
      const record = Buffer.alloc(5)
      record.writeUInt32LE(0x3fe | (1 << 10) | (1 << 20))
      record[4] = byte
      const changed = Buffer.concat([data.subarray(0, end), record, data.subarray(end)])
      return rewriteStreams(opened, new Map([[path, changed]]))!
    }
    const opaque = withRecord(67, 1)
    const afterText = digest(opaque)
    expect(digest(withRecord(67, 2))).not.toBe(afterText)
    expect(digest(withRecord(66, 1))).not.toBe(afterText)
    // Patch 0047 keeps the record with the source paragraph and leaves it out of the clipboard,
    // so copying the text beside it yields plain text and pasting it adds no second record.
    const opaqueRecords = (bytes: Uint8Array) =>
      recordsOf(streamOf(openHwp(bytes)!, SECTION(0))!)!.filter(({ tag }) => tag === 0x3fe).length
    expect(opaqueRecords(opaque)).toBe(1)
    for (const bytes of [source, opaque]) {
      const doc = new HwpDocument(bytes)
      try {
        doc.copySelection(0, 1, 0, 1, 2)
        expect(JSON.parse(doc.getCollabClipboardText()).text).toBe('보존')
        expect(JSON.parse(doc.pasteInternal(0, 0, 0)).ok).toBe(true)
        expect(doc.getTextRange(0, 0, 0, 2)).toBe('보존')
        expect(opaqueRecords(doc.exportHwp())).toBe(bytes === opaque ? 1 : 0)
      } finally {
        doc.free()
      }
    }
  } finally {
    seed.free()
  }
})

it('fingerprints each retained HWPX field and resolves inherited namespace bindings', async () => {
  const seed = HwpDocument.createEmpty()
  try {
    seed.createBlankDocument()
    seed.createTable(0, 0, 0, 1, 1)
    const source = seed.exportHwpx()
    const section = 'Contents/section0.xml'
    const change = async (part: string, mutate: (xml: string) => string) => {
      const zip = await JSZip.loadAsync(source)
      const original = await zip.file(part)!.async('string')
      const changed = mutate(original)
      expect(changed).not.toBe(original)
      zip.file(part, changed, { createFolders: false })
      return digest(await zip.generateAsync({ type: 'uint8array' }))
    }
    for (const [part, marker, first, second] of [
      [section, '<hp:grid ', '<hp:grid strikeContinue="0" ', '<hp:grid strikeContinue="1" '],
      [
        section,
        '</hp:tbl>',
        '<hp:label topMargin="1"/></hp:tbl>',
        '<hp:label topMargin="2"/></hp:tbl>',
      ],
      [
        'Contents/header.xml',
        '<hh:paraPr ',
        '<hh:paraPr textDir="LTR" ',
        '<hh:paraPr textDir="RTL" ',
      ],
    ] as const) {
      expect(await change(part, (xml) => xml.replace(marker, first))).not.toBe(
        await change(part, (xml) => xml.replace(marker, second)),
      )
    }
    const attribute = (xml: string, uri: string, local: boolean) =>
      xml
        .replace(local ? '<hp:p ' : '<hs:sec ', `$&xmlns:x="${uri}" `)
        .replace('<hp:p ', '<hp:p x:mark="1" ')
    const inherited = await change(section, (xml) => attribute(xml, 'urn:one', false))
    expect(await change(section, (xml) => attribute(xml, 'urn:one', true))).toBe(inherited)
    expect(await change(section, (xml) => attribute(xml, 'urn:two', false))).not.toBe(inherited)
  } finally {
    seed.free()
  }
})
