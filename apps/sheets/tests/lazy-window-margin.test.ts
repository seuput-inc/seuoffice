/**
 * The loaded window follows the screen only when the screen nears its edge: a
 * scroll inside the window reads nothing from the file, one that comes within
 * the margin of the window's edge reads the window around the screen.
 */
import { afterEach, describe, expect, it, vi } from 'vitest'
import { createEditJournal } from '../src/renderer/edit-journal'
import { loadVisibleRange } from '../src/renderer/univer-sync'
import type {
  LazyWorkbookState,
  UniverRuntime,
  UniverWorksheet,
} from '../src/renderer/univer-state'

afterEach(() => {
  vi.unstubAllGlobals()
})

type Range = { startRow: number; endRow: number; startColumn: number; endColumn: number }
const at = (startRow: number, startColumn: number, endRow: number, endColumn: number): Range => ({
  startRow,
  endRow,
  startColumn,
  endColumn,
})

/** a 5,000-row sheet whose rows 220-420 are loaded, with the screen at `shown` */
function scrolledTo(shown: Range) {
  const readWorkbookRange = vi.fn().mockRejectedValue(new Error('this test has no file'))
  vi.stubGlobal('window', { desktopApi: { readWorkbookRange } })
  const state = {
    file: { sessionId: 'session', sheets: [{ id: 's1', rowCount: 5_000, columnCount: 20 }] },
    editJournal: createEditJournal(),
    loadedRanges: new Map<string, Range>([['s1', at(220, 0, 420, 19)]]),
    loadingKeys: new Map<string, string>(),
    retryTimers: new Map<string, number>(),
    decorationsPendingSheets: new Set<string>(),
    flags: { preloadRunning: false, preloadComplete: false },
    formulaMode: false,
  } as unknown as LazyWorkbookState
  const worksheet = {
    getSheetId: () => 's1',
    getVisibleRange: () => shown,
  } as unknown as UniverWorksheet
  const runtime = { univerAPI: { getActiveWorkbook: () => null } } as unknown as UniverRuntime
  return { readWorkbookRange, state, worksheet, runtime }
}

describe('the loaded window', () => {
  it('stays put while the screen is well inside it', async () => {
    // the window around this screen (230-430) is not the loaded one, but the screen is inside it
    const { readWorkbookRange, state, worksheet, runtime } = scrolledTo(at(310, 0, 350, 12))
    await loadVisibleRange(runtime, { current: state }, worksheet, () => {})
    expect(readWorkbookRange).not.toHaveBeenCalled()
  })

  it('moves to the screen once the screen nears its edge', async () => {
    const { readWorkbookRange, state, worksheet, runtime } = scrolledTo(at(370, 0, 410, 12))
    await loadVisibleRange(runtime, { current: state }, worksheet, () => {}).catch(() => {})
    expect(readWorkbookRange).toHaveBeenCalledWith(
      expect.objectContaining({ range: at(290, 0, 490, 19) }),
    )
  })

  it('moves to the screen when a load in flight elsewhere would replace it', async () => {
    // a fast scroll asked for rows 305-505, and the screen came back before they landed: once
    // they land they replace 220-420, so the screen needs its own window
    const { readWorkbookRange, state, worksheet, runtime } = scrolledTo(at(310, 0, 350, 12))
    state.loadingKeys.set('s1', '305:505:0:19')
    await loadVisibleRange(runtime, { current: state }, worksheet, () => {}).catch(() => {})
    expect(readWorkbookRange).toHaveBeenCalledWith(
      expect.objectContaining({ range: at(230, 0, 430, 19) }),
    )
  })

  it('stays put while the load in flight is the window around the screen', async () => {
    const { readWorkbookRange, state, worksheet, runtime } = scrolledTo(at(310, 0, 350, 12))
    state.loadingKeys.set('s1', '220:420:0:19')
    await loadVisibleRange(runtime, { current: state }, worksheet, () => {})
    expect(readWorkbookRange).not.toHaveBeenCalled()
  })
})
