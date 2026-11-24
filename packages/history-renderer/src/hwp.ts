import { HistoryError } from '@seuoffice/collab/client'
import { HwpDocument } from '../../../vendor/rhwp-core/rhwp.js'
import {
  applyHwpOp,
  captureHwpUndo,
  decodeHwpOp,
  encodeHwpOp,
  type HwpOp,
} from '../../../apps/hwp/src/shared/collab-editor-ops'
import { releaseHwpUndo } from '../../../apps/hwp/src/shared/collab-text-undo'
import type { ByteHistoryAdapter } from './adapter'
import { openRestorableHwp } from './hwp-source'

export async function openHwpHistory(
  format: 'hwp' | 'hwpx',
  base: Uint8Array,
): Promise<ByteHistoryAdapter<HwpOp>> {
  const file = await openRestorableHwp(format, base, HwpDocument)
  let dirty = false
  return {
    decode: decodeHwpOp,
    encode: encodeHwpOp,
    integrate(ops, local) {
      if (local.length) throw new HistoryError('invalid_history', 'unexpected pending HWP edits')
      for (const op of ops) {
        applyHwpOp(file.doc, op)
        dirty = true
      }
      return local
    },
    captureUndo: (op) => captureHwpUndo(file.doc, op),
    async produceFile() {
      return { fileName: `history.${format}`, bytes: dirty ? await file.produce() : base }
    },
    dispose() {
      releaseHwpUndo(file.doc)
      file.doc.free()
    },
  }
}
