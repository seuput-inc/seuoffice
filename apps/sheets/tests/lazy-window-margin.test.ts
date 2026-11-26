/**
 * The loaded window follows the screen only when the screen nears its edge: a
 * scroll inside the window reads nothing from the file, one that comes within
 * the margin of the window's edge reads the window around the screen.
 */
import { afterEach, describe, expect, it, vi } from 'vitest'
import { createEditJournal } from '../src/renderer/edit-journal'
import { ensureLazyRangeLoaded, loadVisibleRange } from '../src/renderer/univer-sync'
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

/**
 * a 5,000-row sheet whose rows 220-420 are loaded, with the screen at `screen.shown`; reads
 * reject unless `pending`, when they never answer (a load still in flight)
 */
function scrolledTo(shown: Range, pending = false) {
  const readWorkbookRange = pending
    ? vi.fn(() => new Promise(() => {}))
    : vi.fn().mockRejectedValue(new Error('this test has no file'))
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
  const screen = { shown }
  const worksheet = {
    getSheetId: () => 's1',
    getVisibleRange: () => screen.shown,
  } as unknown as UniverWorksheet
  const runtime = { univerAPI: { getActiveWorkbook: () => null } } as unknown as UniverRuntime
  return { readWorkbookRange, state, worksheet, runtime, screen }
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

  it('moves to the screen when the screen comes back before a load elsewhere lands', async () => {
    // the screen nears the edge: the window around it (290-490) is asked for and stays in flight
    const { readWorkbookRange, state, worksheet, runtime, screen } = scrolledTo(
      at(370, 0, 410, 12),
      true,
    )
    void loadVisibleRange(runtime, { current: state }, worksheet, () => {})
    await vi.waitFor(() => expect(readWorkbookRange).toHaveBeenCalledTimes(1))
    // back inside 220-420 but not 290-490: once that lands it replaces 220-420, so the screen
    // needs its own window
    screen.shown = at(250, 0, 290, 12)
    void loadVisibleRange(runtime, { current: state }, worksheet, () => {})
    await vi.waitFor(() =>
      expect(readWorkbookRange).toHaveBeenLastCalledWith(
        expect.objectContaining({ range: at(170, 0, 370, 19) }),
      ),
    )
  })

  it('stays put while the load in flight holds the screen too', async () => {
    const { readWorkbookRange, state, worksheet, runtime, screen } = scrolledTo(
      at(370, 0, 410, 12),
      true,
    )
    void loadVisibleRange(runtime, { current: state }, worksheet, () => {})
    await vi.waitFor(() => expect(readWorkbookRange).toHaveBeenCalledTimes(1))
    // inside both 220-420 and 290-490, with the margin
    screen.shown = at(330, 0, 370, 12)
    await loadVisibleRange(runtime, { current: state }, worksheet, () => {})
    expect(readWorkbookRange).toHaveBeenCalledTimes(1)
  })
})

describe('a load something waits on (an AI read, a bulk edit)', () => {
  it('is not replaced by a scroll, and the screen loads once it is done', async () => {
    const { readWorkbookRange, state, worksheet, runtime, screen } = scrolledTo(at(310, 0, 350, 12))
    // the waited read answers only when the test says so; every later read fails at once
    let answer: (error: Error) => void = () => {}
    readWorkbookRange.mockImplementationOnce(() => new Promise((_, reject) => (answer = reject)))
    const ref = { current: state }
    const waited = ensureLazyRangeLoaded(runtime, ref, worksheet, at(3000, 0, 3010, 5), () => {})
    await vi.waitFor(() => expect(readWorkbookRange).toHaveBeenCalledTimes(1))
    // a small scroll inside the loaded window, which a different load in flight would replace
    screen.shown = at(250, 0, 290, 12)
    await loadVisibleRange(runtime, ref, worksheet, () => {}).catch(() => {})
    expect(readWorkbookRange).toHaveBeenCalledTimes(1)
    // it lands where it was asked for, which takes the window away from the screen
    state.loadedRanges.set('s1', at(3000, 0, 3010, 5))
    answer(new Error('this test has no file'))
    await waited
    // no further scroll: the screen gets its window by itself
    await vi.waitFor(() =>
      expect(readWorkbookRange).toHaveBeenLastCalledWith(
        expect.objectContaining({ range: at(170, 0, 370, 19) }),
      ),
    )
  })
})
