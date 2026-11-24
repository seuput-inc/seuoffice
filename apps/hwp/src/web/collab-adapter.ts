import init, { HwpDocument } from '@rhwp/core'
import type { CollabAdapter } from '@seuoffice/collab/client'
import { openRestorableHwp } from '../../../../packages/history-renderer/src/hwp-source'
import type { HwpStudioCallbacks, HwpStudioControls } from '../../studio-plugin/collab-input.mjs'
import type { HwpSelection } from '../../studio-plugin/collab-presence.mjs'
import type { HwpRecovery } from '../renderer/collab-recovery'
import { t } from '../renderer/i18n/locale'
import { applyHwpCommand, captureHwpCommand } from '../shared/collab-command'
import { releaseHwpUndo } from '../shared/collab-text-undo'
import {
  applyHwpOp,
  captureHwpUndo,
  decodeHwpOp,
  encodeHwpOp,
  type HwpOp,
} from '../shared/collab-editor-ops'
import { applyHwpMemo, captureHwpMemo } from '../shared/collab-memo'
import { applyHwpStructure, captureHwpStructure } from '../shared/collab-structure'
import {
  captureHwpCaret,
  captureHwpText,
  restoreHwpCaret,
  richTextFragment,
} from '../shared/collab-text-native'
import { rebaseHwpOps } from '../shared/collab-transform'

type Format = 'hwp' | 'hwpx'
type Native = HwpDocument & {
  getCollabFingerprint(): string
  forkCollabDocument(): HwpDocument
  restoreCollabConfirmed(confirmed: HwpDocument): void
}

function native(doc: HwpDocument): Native {
  if (
    !('getCollabFingerprint' in doc) ||
    typeof doc.getCollabFingerprint !== 'function' ||
    !('forkCollabDocument' in doc) ||
    typeof doc.forkCollabDocument !== 'function' ||
    !('restoreCollabConfirmed' in doc) ||
    typeof doc.restoreCollabConfirmed !== 'function'
  ) {
    throw new Error('The HWP engine cannot replay collaborative edits')
  }
  return doc as Native
}

function selectedCaret(doc: HwpDocument, position: HwpSelection['head']) {
  try {
    return captureHwpCaret(doc, position)
  } catch {
    return null
  }
}

export interface PreparedHwpCollab {
  readonly format: Format
  readonly bytes: Uint8Array
  readonly fingerprint: string
  produce(doc: HwpDocument): Promise<Uint8Array>
  dispose(): void
}

export async function prepareHwpCollab(
  format: Format,
  bytes: Uint8Array,
): Promise<PreparedHwpCollab> {
  await init()
  const source = await openRestorableHwp(format, bytes, HwpDocument)
  let disposed = false
  try {
    const fingerprint = native(source.doc).getCollabFingerprint()
    return {
      format,
      bytes: source.bytes,
      fingerprint,
      produce(doc) {
        if (disposed) throw new Error('The HWP editor is closed')
        return source.produce(doc)
      },
      dispose() {
        disposed = true
      },
    }
  } finally {
    source.doc.free()
  }
}

export interface HwpCollabBinding {
  adapter: CollabAdapter<HwpOp>
  callbacks: HwpStudioCallbacks
  dispose(): void
}

/** Room one op may take: a replay request carries at most 256 KiB of strings, header included. */
export const MAX_HWP_OP_BYTES = 192 * 1024

/** Why an op cannot join the retained history, said to the user, or null when it can. */
function refusal(format: Format, op: HwpOp): string | null {
  const picture =
    op.kind === 'insertCellPicture' || (op.kind === 'command' && op.action === 'image') ? op : null
  // HWP source admission keeps only PNG, JPEG, GIF and BMP pictures.
  if (format === 'hwp' && picture?.extension === 'webp') return t('collabWebpInHwp')
  if (new TextEncoder().encode(JSON.stringify(encodeHwpOp(op))).byteLength > MAX_HWP_OP_BYTES)
    return t('collabOpTooLarge', { limit: `${Math.floor((MAX_HWP_OP_BYTES * 3) / 4 / 1024)} KB` })
  return null
}

type Removal = { section: number; paragraph: number; control: number; col: boolean; index: number }
function removal(op: HwpOp): Removal | 'paragraph' | null {
  if (op.kind === 'deleteParagraph') return 'paragraph'
  if (op.kind === 'deleteRow') return { ...op, col: false, index: op.row }
  if (op.kind === 'command' && op.action === 'grid' && op.side === 'delete')
    return { ...op.at, control: op.control, col: op.axis === 'column', index: op.index }
  return null
}

/** The text each pending row, column or paragraph deletion would remove, read before it applies. */
function targetsOf(mirror: HwpDocument, pending: readonly HwpOp[]): Map<HwpOp, string> {
  const result = new Map<HwpOp, string>()
  if (!pending.some(removal)) return result
  const doc = native(mirror).forkCollabDocument()
  try {
    for (const op of pending) {
      const target = removal(op)
      if (target === 'paragraph' && op.kind === 'deleteParagraph') {
        const length = doc.getParagraphLength(op.section, op.paragraph)
        result.set(op, doc.getTextRange(op.section, op.paragraph, 0, length))
      } else if (target && target !== 'paragraph') {
        const { section, paragraph, control, col, index } = target
        const { cellCount } = JSON.parse(doc.getTableDimensions(section, paragraph, control))
        const cells: string[] = []
        for (let cell = 0; cell < cellCount; cell++) {
          const info = JSON.parse(doc.getCellInfo(section, paragraph, control, cell))
          if ((col ? info.col : info.row) !== index) continue
          const lines: string[] = []
          const count = doc.getCellParagraphCount(section, paragraph, control, cell)
          for (let p = 0; p < count; p++) {
            const length = doc.getCellParagraphLength(section, paragraph, control, cell, p)
            lines.push(doc.getTextInCell(section, paragraph, control, cell, p, 0, length))
          }
          cells.push(lines.join('\n'))
        }
        result.set(op, cells.join('\t'))
      }
      applyHwpOp(doc, op)
    }
  } finally {
    doc.free()
  }
  return result
}

export function createHwpCollabAdapter({
  file,
  fileName,
  controls,
  onLost,
  notice,
}: {
  file: PreparedHwpCollab
  fileName: string
  controls: HwpStudioControls
  onLost: HwpRecovery['show']
  notice(message: string): void
}): HwpCollabBinding {
  // wasm-bindgen document pointers belong to the frame's instance, not the embedding page's instance.
  const mirror = controls.withDocument((doc) => {
    if (native(doc).getCollabFingerprint() !== file.fingerprint)
      throw new Error('Studio opened a different HWP document')
    return native(doc).forkCollabDocument()
  })
  let disposed = false
  let editable = false
  let buffered = false
  let emit: ((ops: HwpOp[]) => void) | undefined
  let bufferChanged: ((next: boolean) => void) | undefined
  // Reuse confirmed state and pending operands for rollback instead of cloning on every keystroke.
  let pending: HwpOp[] = []
  let staged: HwpOp[] | undefined
  let mutated = false
  function publish(ops: HwpOp[]) {
    if (!emit || disposed || !editable) throw new Error('The HWP document is not editable')
    for (const op of ops) pending.push(op)
    emit(ops)
  }
  function transaction<T>(doc: HwpDocument, action: () => T): T {
    if (staged) return action()
    const ops: HwpOp[] = []
    staged = ops
    mutated = false
    let result: T
    try {
      result = action()
    } catch (error) {
      if (mutated) {
        native(doc).restoreCollabConfirmed(mirror)
        for (const op of pending) applyHwpOp(doc, op)
      }
      throw error
    } finally {
      staged = undefined
      mutated = false
    }
    if (ops.length) publish(ops)
    return result
  }
  type Picture = Extract<HwpOp, { kind: 'insertCellPicture' }>
  type Reason = 'deleted' | 'changed' | 'composition'
  const recover = (reason: Reason, text: string, picture?: Picture) => {
    const reinsert = () => {
      if (!editable || buffered || !emit || disposed) return false
      let op: HwpOp | null = null
      controls.replay(
        (doc, selection) => {
          if (!selection) return
          const at = captureHwpCaret(doc, selection.head).at
          // Place recovered content at the current caret; never replace a new selection.
          op = picture
            ? {
                ...picture,
                at,
                anchor: at.path.length ? 'cell' : 'paper',
                offsetXHu: null,
                offsetYHu: null,
              }
            : decodeHwpOp({ kind: 'splice', from: { ...at }, to: { ...at }, text })
          applyHwpOp(doc, op)
        },
        (before) => before,
      )
      if (!op) return false
      publish([op])
      return true
    }
    if (picture) {
      const bytes = Uint8Array.from(atob(picture.data), (c) => c.charCodeAt(0))
      const ext = picture.extension === 'jpg' ? 'jpeg' : picture.extension
      onLost(
        {
          kind: 'picture',
          blob: new Blob([bytes], { type: `image/${ext}` }),
          name: picture.description,
        },
        reinsert,
        reason,
      )
    } else if (text) onLost({ kind: 'text', text }, reinsert, reason)
  }
  /** `removed` is the text a retried deletion would have removed, offered for copying. */
  function rejected(op: HwpOp, reason: Reason, removed?: string) {
    const changed = (key: Parameters<typeof t>[0]) =>
      notice(t('collabNotApplied', { action: t(key) }))
    if (op.kind === 'insertCellPicture') return recover(reason, '', op)
    if (removed) return onLost({ kind: 'text', text: removed }, null, 'kept')
    if (op.kind === 'deleteRow') return changed('collabTableAction')
    if (op.kind === 'deleteParagraph') return changed('collabDeleteAction')
    if (op.kind === 'command') {
      switch (op.action) {
        case 'characters':
        case 'paragraph':
          return changed('collabFormatAction')
        case 'table':
        case 'grid':
          return changed('collabTableAction')
        case 'picture':
          return changed('collabPictureAction')
        case 'memoRemove':
          return changed('collabMemoAction')
        case 'memoText':
          return op.text ? recover(reason, op.text) : changed('collabMemoAction')
        case 'pasteText':
          return recover(reason, op.fragment.text)
        case 'form':
          changed('collabFormAction')
          for (const value of [op.props.text, op.props.caption]) if (value) recover(reason, value)
          return
        case 'image':
          return recover(reason, '', {
            kind: 'insertCellPicture',
            at: op.at,
            anchor: 'paper',
            data: op.data,
            extension: op.extension,
            description: '',
            naturalWidthPx: op.widthPx,
            naturalHeightPx: op.heightPx,
            width: op.widthPx * 75,
            height: op.heightPx * 75,
            offsetXHu: null,
            offsetYHu: null,
          })
      }
    }
    const text = op.kind === 'nativeText' ? richTextFragment(op) : op.text
    if (text) recover(reason, text)
    else changed(op.kind === 'insertMemo' ? 'collabMemoAction' : 'collabTextAction')
  }
  return {
    adapter: {
      encode: encodeHwpOp,
      decode: decodeHwpOp,
      listen(next) {
        emit = next
        return () => {
          emit = undefined
          bufferChanged = undefined
        }
      },
      observeLocalBuffer(changed) {
        bufferChanged = changed
        changed(buffered)
      },
      integrate(remote, local) {
        let mapped: HwpSelection | null = null
        const removed = targetsOf(mirror, local)
        const plan = controls.replay(
          (doc, before) => {
            const anchor = before && selectedCaret(doc, before.anchor)
            const head = before && selectedCaret(doc, before.head)
            const plan = rebaseHwpOps(mirror, remote, local)
            if (local.length) {
              native(doc).restoreCollabConfirmed(mirror)
              for (const op of plan.replay) applyHwpOp(doc, op)
            } else {
              for (const op of remote) applyHwpOp(doc, op)
            }
            if (anchor && head) {
              const a = plan.mapCaret(anchor.at),
                h = plan.mapCaret(head.at)
              const nextAnchor = a && restoreHwpCaret(doc, { at: a, slot: anchor.slot })
              const nextHead = h && restoreHwpCaret(doc, { at: h, slot: head.slot })
              if (nextAnchor && nextHead) mapped = { anchor: nextAnchor, head: nextHead }
            }
            return plan
          },
          () => mapped,
        )
        pending = plan.replay.slice()
        for (const op of plan.deleted) rejected(op, 'deleted')
        // Only a deletion kept for a peer's confirmed content offers its text to copy.
        for (const op of plan.retry)
          rejected(op, 'changed', plan.kept.includes(op) ? removed.get(op) : undefined)
        return plan.replay
      },
      confirmed(ops) {
        for (const op of ops) applyHwpOp(mirror, op)
        pending.splice(0, ops.length)
      },
      captureUndo: (op) => captureHwpUndo(mirror, op),
      fingerprint: () => controls.withDocument((doc) => native(doc).getCollabFingerprint()),
      setEditable(value) {
        editable = !disposed && value
        controls.setEditable(editable)
      },
      async produceFile() {
        if (buffered) throw new Error('Finish the current text composition before exporting')
        // Unmount may free Studio and its mirror while ZIP checks yield. Own this export's model.
        const copy = controls.withDocument((doc) => native(doc).forkCollabDocument())
        try {
          return { fileName, bytes: await file.produce(copy) }
        } finally {
          copy.free()
        }
      },
    },
    callbacks: {
      transaction,
      onBuffer(next) {
        buffered = next
        bufferChanged?.(next)
      },
      onDraftLost: (text) => recover('composition', text),
      onCaretLost: () => notice(t('collabCaretLost')),
      onMutation(doc, method, args, call) {
        if (!editable || disposed || !emit) throw new Error('The HWP document is not editable')
        const memo =
          method === 'insertMemo' || method === 'insertMemoByPath'
            ? captureHwpMemo(method, args, new Date())
            : null
        const structure = memo ? null : captureHwpStructure(doc, method, args)
        const op =
          memo ??
          structure ??
          captureHwpText(doc, method, args) ??
          captureHwpCommand(doc, method, args)
        if (!op) throw new Error(t('collabUnsupportedEdit'))
        const refused = refusal(file.format, op)
        if (refused) throw new Error(refused)
        return transaction(doc, () => {
          mutated = true
          const result = memo
            ? applyHwpMemo(doc, memo)
            : structure
              ? applyHwpStructure(doc, structure, call).result
              : op.kind === 'command'
                ? applyHwpCommand(doc, op)
                : call()
          if (typeof result !== 'string' || JSON.parse(result)?.ok !== true)
            throw new Error(t('collabRefusedCommand'))
          staged!.push(op)
          return result
        })
      },
    },
    dispose() {
      if (disposed) return
      disposed = true
      emit = bufferChanged = undefined
      releaseHwpUndo(mirror)
      mirror.free()
    },
  }
}
