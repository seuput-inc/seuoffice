import type { HwpDocument } from '@rhwp/core'
import { HistoryError, type Json } from '@seuoffice/collab/client'
import { applyHwpCommand, parseHwpCommand, type HwpCommandOp } from './collab-command'
import { applyHwpMemo, parseHwpMemo, type HwpMemoInsert } from './collab-memo'
import { applyHwpStructure, parseHwpStructure, type HwpStructureOp } from './collab-structure'
import { parseHwpTextSplice, type HwpTextSplice } from './collab-text'
import { applyHwpText } from './collab-text-model'
import { applyHwpRichText, parseHwpRichText, type HwpRichTextOp } from './collab-text-native'
import {
  applyHwpTextFragment,
  captureHwpTextUndo,
  parseHwpTextFragment,
  type HwpTextFragment,
} from './collab-text-undo'
import { encodeHwpText } from './collab-text-wire'

export type HwpOp =
  HwpTextSplice | HwpMemoInsert | HwpStructureOp | HwpRichTextOp | HwpCommandOp | HwpTextFragment

export function textOp(op: HwpOp): op is HwpTextSplice {
  return op.kind === 'splice' || op.kind === 'lineBreak'
}

export function decodeHwpOp(value: Json): HwpOp {
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    let op: HwpOp | null
    switch (value.kind) {
      case 'insertMemo':
        op = parseHwpMemo(value)
        break
      case 'nativeText':
        op = parseHwpRichText(value)
        break
      case 'command':
        op = parseHwpCommand(value)
        break
      case 'restoreText':
        op = parseHwpTextFragment(value)
        break
      case 'splice':
      case 'lineBreak':
        op = parseHwpTextSplice(value)
        break
      default:
        op = parseHwpStructure(value)
    }
    if (op) return op
  }
  throw new HistoryError('invalid_history', 'invalid HWP operation')
}

export function encodeHwpOp(op: HwpOp): Json {
  if (textOp(op)) return encodeHwpText(op)
  if ('at' in op) return { ...op, at: { ...op.at } }
  return op
}

/** No operation from the wire can select a method on the WASM object. */
export function applyHwpOp(doc: HwpDocument, op: HwpOp) {
  if (textOp(op)) return applyHwpText(doc, op)
  if (op.kind === 'nativeText') return applyHwpRichText(doc, op)
  if (op.kind === 'command') {
    applyHwpCommand(doc, op)
    return
  }
  if (op.kind === 'restoreText') return applyHwpTextFragment(doc, op)
  if (op.kind === 'insertMemo') {
    const reply = applyHwpMemo(doc, op)
    if (JSON.parse(reply)?.ok !== true) throw new Error('HWP refused the memo')
    return
  }
  return applyHwpStructure(doc, op)
}

export function captureHwpUndo(doc: HwpDocument, target: HwpOp) {
  return captureHwpTextUndo(doc, target)
}
