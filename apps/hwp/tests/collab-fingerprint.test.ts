import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { beforeAll, expect, it } from 'vitest'
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
