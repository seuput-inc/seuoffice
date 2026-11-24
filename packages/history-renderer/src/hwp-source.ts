import { HistoryError } from '@seuoffice/collab/client'
import type { HwpDocument } from '../../../vendor/rhwp-core/rhwp.js'
import { certifyHwp5Source } from '../../../apps/hwp/src/main/collab-hwp5-proof'
import { certifyHwpxSource } from '../../../apps/hwp/src/shared/collab-hwpx-source'
import { openHwpFile } from './hwp-file'

/** Admission must use the same bytes and reconstruction rules for a live epoch and its replay. */
export async function openRestorableHwp(
  format: 'hwp' | 'hwpx',
  bytes: Uint8Array,
  Document: typeof HwpDocument,
) {
  const file = await openHwpFile(format, bytes, Document)
  try {
    if (format === 'hwpx') await certifyHwpxSource(file.bytes, Document)
    else certifyHwp5Source(file.bytes, Document)
    await file.produce()
    return file
  } catch (error) {
    file.doc.free()
    throw new HistoryError(
      'unsupported',
      error instanceof Error ? error.message : 'HWP cannot preserve this source',
    )
  }
}
