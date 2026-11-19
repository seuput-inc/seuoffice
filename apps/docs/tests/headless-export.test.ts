import { afterEach, describe, expect, it, vi } from 'vitest'

import {
  fontFacesIdle,
  runHeadlessDocumentExport,
  waitForDocumentSettled,
} from '../src/renderer/headless-export'
import type { DocumentReadiness } from '../src/renderer/headless-export'

/**
 * Headless export readiness (src/renderer/headless-export.ts): the hidden
 * window must not print before a document is open and pagination has
 * settled, and it must fail loudly instead of hanging when it never does.
 */

type PageDebugWindow = typeof globalThis & { __pageDebug?: { pageCutSig?: string } }

/** a PageCut signature with `count` pages (the e2e-visible shape, one entry per page) */
function setSlices(count: number | null): void {
  const w = globalThis as PageDebugWindow
  if (count === null) delete w.__pageDebug
  else
    w.__pageDebug = {
      pageCutSig: Array.from({ length: count }, (_, i) => `${i}:${i + 1}`).join('|'),
    }
}

function setSig(sig: string): void {
  ;(globalThis as PageDebugWindow).__pageDebug = { pageCutSig: sig }
}

afterEach(() => {
  setSlices(null)
  vi.restoreAllMocks()
})

const fast = { pollMs: 1, timeoutMs: 400 }

/** default readiness: a real file is open and the open did not fail */
const ready = (over: Partial<DocumentReadiness> = {}): DocumentReadiness => ({
  opened: true,
  failed: false,
  ...over,
})

describe('waitForDocumentSettled', () => {
  it('waits for a document and a page count that repeats', async () => {
    setSlices(null)
    let hasDoc = false
    setTimeout(() => {
      hasDoc = true
      setSlices(2)
    }, 5)
    await waitForDocumentSettled(() => ready({ opened: hasDoc }), fast)
    expect(hasDoc).toBe(true)
  })

  it('keeps waiting while the page count is still moving', async () => {
    setSlices(1)
    const growth = [2, 3, 4, 4, 4]
    let i = 0
    const timer = setInterval(() => {
      if (i < growth.length) setSlices(growth[i++]!)
    }, 2)
    await waitForDocumentSettled(ready, fast)
    clearInterval(timer)
    expect(i).toBeGreaterThan(2)
  })

  it('keeps waiting while a cut moves inside a steady page count', async () => {
    const sigs = ['0:10|10:20', '0:11|11:20', '0:12|12:20', '0:12|12:20', '0:12|12:20']
    let i = 0
    const timer = setInterval(() => {
      if (i < sigs.length) setSig(sigs[i++]!)
    }, 2)
    await waitForDocumentSettled(ready, fast)
    clearInterval(timer)
    expect(i).toBeGreaterThan(3)
  })

  it('fails when no document ever opens', async () => {
    await expect(waitForDocumentSettled(() => ready({ opened: false }), fast)).rejects.toThrow(
      'did not open',
    )
  })

  it('fails when pagination produced no pages at all', async () => {
    setSlices(0)
    await expect(waitForDocumentSettled(ready, fast)).rejects.toThrow(
      'pagination produced no pages',
    )
  })

  it('accepts a page count that never stabilized rather than losing the export', async () => {
    let n = 1
    const timer = setInterval(() => setSlices(++n), 1)
    await expect(waitForDocumentSettled(ready, fast)).resolves.toBeUndefined()
    clearInterval(timer)
  })
})

describe('waitForDocumentSettled for a caller on a short budget', () => {
  /** the page's font set: its faces, and a `ready` that never comes (WebKit after a failed face) */
  function pageFonts(faces: { status: string }[]): void {
    vi.stubGlobal('document', { fonts: Object.assign(faces, { ready: new Promise(() => {}) }) })
  }

  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('settles on a page that never reports its fonts ready, once no face is loading', async () => {
    pageFonts([{ status: 'loaded' }, { status: 'error' }, { status: 'unloaded' }])
    setSlices(2)
    const { signal } = new AbortController()
    await expect(
      waitForDocumentSettled(ready, { ...fast, signal, fontsSettled: () => fontFacesIdle(signal) }),
    ).resolves.toBeUndefined()
  })

  it('waits for a face that is still loading', async () => {
    let asked = 0
    const face = {
      // loading the first three times it is asked
      get status() {
        return ++asked > 3 ? 'loaded' : 'loading'
      },
    }
    pageFonts([{ status: 'loaded' }, face])
    setSlices(2)
    const { signal } = new AbortController()
    await waitForDocumentSettled(ready, {
      ...fast,
      signal,
      fontsSettled: () => fontFacesIdle(signal, 1),
    })
    expect(asked).toBe(4)
  })

  it('stops waiting for a face when the caller gives up', async () => {
    const caller = new AbortController()
    let asked = 0
    const face = {
      // never loaded; the caller gives up the third time it is asked
      get status() {
        if (++asked === 3) caller.abort(new Error('the caller gave up'))
        return 'loading'
      },
    }
    pageFonts([face])
    setSlices(2)
    await expect(
      waitForDocumentSettled(ready, {
        ...fast,
        signal: caller.signal,
        fontsSettled: () => fontFacesIdle(caller.signal, 1),
      }),
    ).rejects.toThrow('the caller gave up')
  })
})

describe('waitForDocumentSettled (unreadable input)', () => {
  it('fails immediately on the blank fallback instead of waiting out the budget', async () => {
    const t0 = Date.now()
    await expect(
      waitForDocumentSettled(() => ready({ opened: false, failed: true }), {
        pollMs: 1,
        timeoutMs: 5000,
      }),
    ).rejects.toThrow('did not open')
    expect(Date.now() - t0).toBeLessThan(1000)
  })
})

describe('runHeadlessDocumentExport', () => {
  it('runs the export once the document has settled', async () => {
    setSlices(3)
    const exportPdf = vi.fn(() => Promise.resolve(true))
    await expect(runHeadlessDocumentExport('/o.pdf', ready, exportPdf, fast)).resolves.toEqual({
      ok: true,
    })
    expect(exportPdf).toHaveBeenCalledWith('/o.pdf')
  })

  it('reports the stall instead of exporting a document that never opened', async () => {
    const exportPdf = vi.fn(() => Promise.resolve(true))
    await expect(
      runHeadlessDocumentExport('/o.pdf', () => ready({ opened: false }), exportPdf, fast),
    ).resolves.toMatchObject({ ok: false, error: expect.stringContaining('did not open') })
    expect(exportPdf).not.toHaveBeenCalled()
  })
})
