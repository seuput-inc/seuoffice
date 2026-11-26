/**
 * A range read from the file waits for the faces its rich-text runs name before the grid
 * installs it (canvas text never fetches a face): the alias faces of their families and, for
 * italic runs, the italic faces, together and no longer than the one font-load cap.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createEditJournal } from '../src/renderer/edit-journal'
import { readSheetRangeMapped } from '../src/renderer/univer-sync'
import type { LazyWorkbookState } from '../src/renderer/univer-state'

const sheet = { id: 's1', rowCount: 100, columnCount: 5 }

/** a sheet whose only cell's run is `run`, with every font load hanging */
function fileWith(run: { family: string; italic: boolean }) {
  const readWorkbookRange = vi.fn().mockResolvedValue({
    cells: [{ row: 0, column: 0, value: 'x', rich: [{ text: 'x', bold: false, ...run }] }],
    rows: [],
    merges: [],
    hyperlinks: [],
    indexedThroughRow: 99,
    indexingComplete: true,
  })
  const hanging = new Promise(() => {})
  vi.stubGlobal('window', { desktopApi: { readWorkbookRange } })
  vi.stubGlobal(
    'FontFace',
    class {
      load() {
        return hanging
      }
    },
  )
  vi.stubGlobal('document', { fonts: { add() {}, load: () => hanging } })
  const state = {
    file: { sessionId: 'session', sheets: [sheet] },
    editJournal: createEditJournal(),
  } as unknown as LazyWorkbookState
  return () =>
    readSheetRangeMapped(
      state,
      's1',
      { startRow: 0, endRow: 0, startColumn: 0, endColumn: 0 },
      sheet as never,
    )
}

/** ms until `read` settles, in fake time */
async function settlesAfter(read: () => Promise<unknown>): Promise<number> {
  let done = false
  void read().then(() => (done = true))
  for (let ms = 0; ms <= 10_000; ms += 100) {
    await vi.advanceTimersByTimeAsync(100)
    if (done) return ms + 100
  }
  return Infinity
}

beforeEach(() => vi.useFakeTimers())
afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
})

describe('a range read with rich-text runs', () => {
  it('waits for the alias faces of the families its runs name', async () => {
    expect(await settlesAfter(fileWith({ family: 'Cambria', italic: false }))).toBe(3_000)
  })

  it('waits for those and the italic faces together, within one cap', async () => {
    expect(await settlesAfter(fileWith({ family: 'Garamond', italic: true }))).toBe(3_000)
  })
})
