import { Buffer } from 'node:buffer'
import { types } from 'node:util'
import { HistoryError, type HistoryResult } from '@seuoffice/collab/client'
import { launch } from './sandbox.js'
export { renderHistory, renderStream } from './stream-host.js'

/** A UTF-8 JSON HistoryRequest is opaque to the credentialed Node process. */
export async function renderBytes(
  base: Uint8Array,
  requestJson: Uint8Array,
): Promise<HistoryResult> {
  if (
    !types.isUint8Array(base) ||
    !types.isUint8Array(requestJson) ||
    !base.byteLength ||
    base.byteLength > 2 * 1024 * 1024 ||
    !requestJson.byteLength ||
    requestJson.byteLength > 1024 * 1024
  )
    throw new HistoryError('invalid_history', 'input frame exceeds byte limits')
  const size = Buffer.alloc(4)
  size.writeUInt32LE(base.byteLength)
  const output = await launch(
    new URL('guest.js', import.meta.url),
    Buffer.concat([size, base, requestJson]),
  )
  if (output.length < 4) throw new HistoryError('invalid_history', 'incomplete sandbox result')
  const end = 4 + output.readUInt32LE(0)
  if (end > output.length || end > 1024 * 1024 + 4)
    throw new HistoryError('invalid_history', 'invalid sandbox result')
  const meta = JSON.parse(output.toString('utf8', 4, end))
  if (meta.error === 'unsupported' || meta.error === 'conflict' || meta.error === 'invalid_history')
    throw new HistoryError(meta.error)
  const bytes = new Uint8Array(
    output.buffer.slice(output.byteOffset + end, output.byteOffset + output.length),
  )
  if (
    !bytes.length ||
    bytes.length > 2 * 1024 * 1024 ||
    !['history.docx', 'history.xlsx', 'history.pptx', 'history.pdf', 'history.hwp', 'history.hwpx'].includes(
      meta.fileName,
    ) ||
    (meta.compensation !== undefined && !Array.isArray(meta.compensation))
  )
    throw new HistoryError('invalid_history', 'invalid sandbox result')
  return meta.compensation
    ? { fileName: meta.fileName, bytes, compensation: meta.compensation }
    : { fileName: meta.fileName, bytes }
}
