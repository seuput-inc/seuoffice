import { HistoryError } from '@seuoffice/collab/client'
import { HwpDocument } from '../../../vendor/rhwp-core/rhwp.js'
import type { HwpTextSplice } from '../../../apps/hwp/src/shared/collab-text'
import { applyHwpText } from '../../../apps/hwp/src/shared/collab-text-model'
import { captureHwpTextUndo } from '../../../apps/hwp/src/shared/collab-text-undo'
import { decodeHwpText, encodeHwpText } from '../../../apps/hwp/src/shared/collab-text-wire'
import type { ByteHistoryAdapter } from './adapter'
import { openHwpFile } from './hwp-file'

export async function openHwpHistory(
  format: 'hwp' | 'hwpx',
  base: Uint8Array,
): Promise<ByteHistoryAdapter<HwpTextSplice>> {
  const file = await openHwpFile(format, base, HwpDocument)
  try {
    // Even version zero must be restorable, but its result is the original bytes.
    await file.produce()
  } catch (error) {
    file.doc.free()
    throw error
  }
  let dirty = false
  return {
    decode: decodeHwpText,
    encode: encodeHwpText,
    integrate(ops, local) {
      if (local.length) throw new HistoryError('invalid_history', 'unexpected pending HWP edits')
      for (const op of ops) {
        applyHwpText(file.doc, op)
        dirty = true
      }
      return local
    },
    captureUndo: (op) => captureHwpTextUndo(file.doc, op),
    async produceFile() {
      return { fileName: `history.${format}`, bytes: dirty ? await file.produce() : base }
    },
    dispose: () => file.doc.free(),
  }
}
