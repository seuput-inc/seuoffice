/**
 * The Sheets page joining the host's co-editing session: the format it
 * declares, and the adapter it builds once the renderer shows the workbook.
 */
import { joinPageCollab, webLanguage, type PageCollab } from '@seuoffice/web-platform'
import { DOCS_NORMAL_EDITOR_UNIT_ID_KEY, ICommandService, type IRange } from '@univerjs/core'
import { IEditorService } from '@univerjs/docs-ui'
import { DeviceInputEventType } from '@univerjs/engine-render'
import { IEditorBridgeService, SetCellEditVisibleOperation } from '@univerjs/sheets-ui'
import { KeyCode } from '@univerjs/ui'
import type { SheetRedactionState } from '@seuoffice/xlsx-gateway/gateway/xlsx-redaction'
import { handleSave } from '../renderer/save-actions'
import { setEditRefusalRule } from '../renderer/sheet-protection'
import { showNotice, showToast } from '../renderer/toast-bus'
import {
  type LazyWorkbookState,
  modelResidency,
  type UniverRuntime,
} from '../renderer/univer-state'
import {
  type BlockReason,
  type SheetOp,
  type SheetsCollabAdapter,
  sheetsCollabAdapter,
} from './collab-adapter'
import {
  INSERT_COL,
  INSERT_ROW,
  MOVE_RANGE,
  type Mutation,
  REMOVE_COL,
  REMOVE_ROWS,
  SET_RANGE_VALUES,
} from './collab-mutations'
import { journalMark, structuralOpsRefusal } from './collab-journal'
import { sheetsPresence } from './collab-presence'
import { refusalIsSilent, sharedWhileCoEditing } from './collab-refusals'

/** What the renderer hands over (`window.seuCollab.attach`) once the opened workbook is on screen */
export interface SheetsRendererHandles {
  readonly runtime: UniverRuntime
  readonly state: LazyWorkbookState
  /** Base-file coordinates; the save shifts these with the structural journal. */
  readonly redactionStates: readonly SheetRedactionState[]
}

/**
 * Bump `schema` when the op format or what the fingerprint covers changes:
 * pages of two schemas must not share a session.
 */
const FORMAT = { name: 'xlsx', schema: 3 }

const noop = (): void => {}

const REFUSALS: Record<BlockReason, { ko: string; en: string }> = {
  unshared: {
    ko: '함께 편집하는 동안에는 이 편집을 할 수 없습니다.',
    en: 'This edit is not available while several people edit the workbook.',
  },
  'too-large': {
    ko: '한 번에 함께 반영하기에는 너무 큰 편집이라 되돌렸습니다. 범위를 나누어 다시 해 주세요.',
    en: 'This edit was too large to share in one step and was taken back. Do it in smaller parts.',
  },
  // the session's journal starts empty again only with a new session: after a save, once
  // nobody has had the workbook open for the server's idle time (packages/collab README)
  limit: {
    ko: '이 편집 세션에서는 행·열·병합·크기 변경을 더 받을 수 없습니다. 저장한 뒤 모두 문서를 닫고, 잠시 뒤에 다시 열어 주세요.',
    en: 'This session cannot take more row, column, merge or size changes. Save, have everyone close the workbook, and open it again a little later.',
  },
}

const refusalText = (reason: BlockReason = 'unshared'): string =>
  REFUSALS[reason][webLanguage() === 'ko' ? 'ko' : 'en']

/** The app's own toast, as for its other refused actions; at most one per second */
let lastRefusal = 0
function refused(id: string, reason: BlockReason = 'unshared'): void {
  console.warn(`[co-editing] refused an edit (${reason}): ${id}`)
  if (Date.now() - lastRefusal < 1_000) return
  lastRefusal = Date.now()
  showToast(refusalText(reason), 'error')
}

const say = (text: { ko: string; en: string }): string => text[webLanguage() === 'ko' ? 'ko' : 'en']

const DRAFT_OVERWRITTEN = {
  ko: '다른 사람이 지금 입력 중인 칸을 바꿨습니다. 입력을 마치면 내 내용으로 바뀝니다.',
  en: 'Someone else changed the cell you are typing in. Finishing your entry replaces their change.',
}

const draftRemoved = (text: string) => ({
  ko: `다른 사람이 입력 중인 칸의 행이나 열을 지워 입력을 취소했습니다. 입력하던 내용: ${text}`,
  en: `Someone else removed the row or column of the cell you were typing in, so your entry was cancelled. You typed: ${text}`,
})

type Cells = Record<number, Record<number, unknown>> | null | undefined

/** Whether `mutation` puts a value, or nothing, into the cell at `row`, `column` of sheet `sheetId` */
function writesCell(mutation: Mutation, sheetId: string, row: number, column: number): boolean {
  const p = mutation.params
  const [sheet, cells] =
    mutation.id === SET_RANGE_VALUES
      ? [p.subUnitId, p.cellValue as Cells]
      : mutation.id === MOVE_RANGE
        ? [(p.to as { subUnitId: string }).subUnitId, (p.to as { value: Cells }).value]
        : [null, null]
  return sheet === sheetId && cells?.[row]?.[column] !== undefined
}

type Axis = 'row' | 'column'

const AXIS_OF: Record<string, { axis: Axis; inserts: boolean }> = {
  [INSERT_ROW]: { axis: 'row', inserts: true },
  [INSERT_COL]: { axis: 'column', inserts: true },
  [REMOVE_ROWS]: { axis: 'row', inserts: false },
  [REMOVE_COL]: { axis: 'column', inserts: false },
}

const lines = (range: IRange, axis: Axis): [number, number] =>
  axis === 'row' ? [range.startRow, range.endRow] : [range.startColumn, range.endColumn]

/** What one `integrate` learns about the cell draft while it runs */
interface DraftWatch {
  written: boolean
  removed: boolean
  /** the id of the mutation of someone else's about to run */
  remoteNext: string | null
  /**
   * The draft's row or column taken back with an unconfirmed insert of this page's: where the
   * lines would come back (`anchor`), how many, and where in them the draft sits
   */
  parked: { axis: Axis; anchor: number; count: number; offset: number } | null
  /** where the draft goes once Univer has moved it along the lines that came back, by `id` */
  back: { id: string; axis: Axis; line: number } | null
}

/**
 * Univer reloads the open cell editor from its cell whenever a mutation writes that cell. During
 * `integrate` that is someone else's change, or this page's unconfirmed edits taken back and run
 * again, and the reload would throw away what the person is typing. The draft stays instead: its
 * commit is a later edit than the one that came in, and the person is told when someone else
 * wrote that cell. Univer moves the edit location along inserted and removed rows and columns,
 * except when its own row or column goes: when someone else removes it, the commit would write
 * over the cell that moved into its place, so the entry is cancelled and a notice that stays
 * shows its text to copy. `beforeRemote` goes to the adapter: each edit of someone else's is
 * compared with where the draft is at the moment it runs.
 *
 * The draft may sit in lines this page inserted and has not had confirmed: taking that insert
 * back removes them until it runs again. The draft is then parked where they come back, out of
 * reach of the edits that arrive meanwhile, and put back into them; lines that do not come back
 * where expected cancel the entry as a removal would.
 */
function keepingCellDraft(runtime: UniverRuntime): {
  beforeRemote: (mutation: Mutation) => void
  around: (integrate: SheetsCollabAdapter['integrate']) => SheetsCollabAdapter['integrate']
} {
  // looked up when an edit first arrives: joining the session touches none of Univer's editor
  let services: ReturnType<typeof editorServices> | undefined
  const use = () => (services ??= editorServices(runtime))
  // null outside an `integrate` that keeps a draft
  let watch: DraftWatch | null = null

  const beforeRemote = (mutation: Mutation): void => {
    if (!watch) return
    const at = use().bridge.getEditLocation()
    if (!at) return
    watch.remoteNext = mutation.id
    if (watch.parked) return
    if (writesCell(mutation, at.sheetId, at.row, at.column)) watch.written = true
  }

  // before each line change runs: Univer moves the location along it afterwards
  const beforeLines = (id: string, params: Mutation['params']): void => {
    const { bridge } = use()
    const remote = watch!.remoteNext === id
    watch!.remoteNext = null
    const kind = AXIS_OF[id]
    const at = bridge.getEditLocation()
    if (!kind || !at || params.subUnitId !== at.sheetId) return
    const { axis, inserts } = kind
    const [start, end] = lines(params.range as IRange, axis)
    const count = end - start + 1
    const parked = watch!.parked
    if (parked?.axis === axis) {
      if (inserts && !remote && start === parked.anchor && count === parked.count) {
        watch!.back = { id, axis, line: start + parked.offset }
        watch!.parked = null
      } else if (inserts && start < parked.anchor) parked.anchor += count
      else if (!inserts && end < parked.anchor) parked.anchor -= count
      else if (!inserts && start < parked.anchor) parked.anchor = start
      return
    }
    if (inserts) return
    const line = axis === 'row' ? at.row : at.column
    if (line < start || line > end) return
    if (remote || parked) watch!.removed = true
    else watch!.parked = { axis, anchor: start, count, offset: line - start }
  }

  const around =
    (integrate: SheetsCollabAdapter['integrate']): SheetsCollabAdapter['integrate'] =>
    (remote, local) => {
      const { injector, bridge, commands } = use()
      const draft = bridge.isVisible().visible ? bridge.getEditLocation() : null
      if (!draft) return integrate(remote, local)
      const { unitId } = draft
      const found: DraftWatch = (watch = {
        written: false,
        removed: false,
        remoteNext: null,
        parked: null,
        back: null,
      })
      const before = commands.beforeCommandExecuted(({ id, params }) =>
        beforeLines(id, (params ?? {}) as Mutation['params']),
      )
      const after = commands.onCommandExecuted(({ id }) => {
        const back = found.back
        const at = back?.id === id && bridge.getEditLocation()
        if (!back || !at) return
        found.back = null
        if (back.axis === 'row') bridge.updateEditLocation(back.line, at.column)
        else bridge.updateEditLocation(at.row, back.line)
      })
      const refresh = bridge.refreshEditCellState
      bridge.refreshEditCellState = noop
      let rebuilt: SheetOp[]
      try {
        rebuilt = integrate(remote, local)
      } finally {
        bridge.refreshEditCellState = refresh
        before.dispose()
        after.dispose()
        watch = null
      }
      if (found.removed || found.parked) {
        const text = (
          injector.get(IEditorService).getEditor(DOCS_NORMAL_EDITOR_UNIT_ID_KEY)?.getDocumentData()
            .body?.dataStream ?? ''
        ).replace(/\r?\n$/, '')
        commands.syncExecuteCommand(SetCellEditVisibleOperation.id, {
          visible: false,
          eventType: DeviceInputEventType.Keyboard,
          keycode: KeyCode.ESC,
          unitId,
        })
        // no clipboard write here: without a press of the person's the browser may refuse it
        showNotice({ text: say(draftRemoved(text)), copy: text })
        return rebuilt
      }
      if (found.written) showToast(say(DRAFT_OVERWRITTEN), 'error')
      return rebuilt
    }
  return { beforeRemote, around }
}

function editorServices(runtime: UniverRuntime) {
  const injector = runtime.univer.__getInjector()
  return {
    injector,
    bridge: injector.get(IEditorBridgeService),
    commands: injector.get(ICommandService),
  }
}

export function joinSheetsCollab(
  fileName: string,
  bytes: Uint8Array,
): Promise<PageCollab<SheetsRendererHandles> | null> {
  return joinPageCollab<SheetsRendererHandles, SheetOp>({
    format: FORMAT,
    fileName,
    bytes,
    presence: sheetsPresence,
    adapter({ runtime, state, redactionStates }) {
      // A streamed workbook holds only the cells on screen, so it cannot replay or save the log;
      // throwing here makes the page report collab_unavailable and the host reopen under its lock.
      if (!state.formulaMode || !state.flags.preloadComplete) {
        throw new Error('this workbook is too large to co-edit; it opens streamed from the file')
      }
      const unitId = runtime.univerAPI.getActiveWorkbook()?.getId()
      if (!unitId) throw new Error('no workbook on screen')
      // Every cell is installed and must stay so: the others' edits apply to this model and its
      // fingerprint is compared with theirs. The windowed loader would evict what scrolls away.
      modelResidency.whole = true
      const refs = { univerRef: { current: runtime }, lazyWorkbookRef: { current: state } }
      const draft = keepingCellDraft(runtime)
      const adapter = sheetsCollabAdapter(
        {
          univer: runtime.univer,
          unitId,
          fileAllowsCellUndo: () =>
            state.formulaTextTruncated.size === 0 &&
            state.file.sheets.every(
              (sheet) =>
                state.formulaText.get(sheet.id)?.size === 0 &&
                state.sheetProtections.get(sheet.id)?.protected === false &&
                sheet.tables.length === 0 &&
                sheet.cellImages.length === 0 &&
                sheet.pivotTables.length === 0 &&
                !sheet.pivotsUnlocated,
            ),
          async produceFile() {
            // 'template' builds the same save request but only writes a copy: no reopen, no re-ided sheets
            const outcome = await handleSave(
              {
                ...refs,
                setMessage: noop,
                openLazyWorkbook: noop,
                stashViewRestore: noop,
                redactionStates: () => redactionStates,
              },
              'template',
              true,
            )
            if (!outcome.ok || !outcome.bytes) throw new Error('the workbook could not be written')
            return { fileName, bytes: outcome.bytes }
          },
          journalMark: (mutation) => journalMark(state.editJournal, mutation),
          refusalOf: (mutation) => structuralOpsRefusal(state.editJournal, mutation),
        },
        { onBlocked: refused, beforeRemote: draft.beforeRemote },
      )
      // From here on the renderer refuses what would stay on this page (collab-refusals.ts)
      setEditRefusalRule((kind) => {
        if (sharedWhileCoEditing(kind)) return null
        if (!refusalIsSilent(kind)) refused(kind)
        return refusalText()
      })
      return { ...adapter, integrate: draft.around(adapter.integrate) }
    },
  })
}
