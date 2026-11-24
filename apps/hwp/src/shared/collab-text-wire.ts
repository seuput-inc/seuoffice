import { HistoryError, type Json } from '@seuoffice/collab/client'
import { parseHwpTextSplice, type HwpTextSplice } from './collab-text'

export function decodeHwpText(json: Json): HwpTextSplice {
  const op = parseHwpTextSplice(json)
  if (!op) throw new HistoryError('invalid_history', 'invalid HWP text operation')
  return op
}

export function encodeHwpText(op: HwpTextSplice): Json {
  return { kind: op.kind, from: { ...op.from }, to: { ...op.to }, text: op.text }
}
