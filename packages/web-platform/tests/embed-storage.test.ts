import { afterEach, describe, expect, it } from 'vitest'
import {
  createEmbedStorage,
  installEmbedStorage,
  purgeLegacyDocumentKeys,
  PERSISTED_KEYS,
} from '../src/embed-storage'

/** A Storage that records what reached it, standing for the origin's real localStorage */
function backingStore(
  initial: Record<string, string> = {},
): Storage & { dump(): Record<string, string> } {
  const data = new Map(Object.entries(initial))
  return {
    get length() {
      return data.size
    },
    key: (index: number) => [...data.keys()][index] ?? null,
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => void data.set(key, String(value)),
    removeItem: (key: string) => void data.delete(key),
    clear: () => data.clear(),
    dump: () => Object.fromEntries(data),
  }
}

const MARKER = 'quarterly-revenue-marker'

describe('the embed store', () => {
  it('keeps a key the page did not allow in memory: nothing reaches the origin', () => {
    const backing = backingStore()
    const store = createEmbedStorage(backing)
    store.setItem('genoffice.sheets.findHistory', JSON.stringify([MARKER]))
    store.setItem(`ai-slides-custom-shows:/web/1/${MARKER}.pptx`, MARKER)
    store.setItem('aidocs.list.recent.bullets', MARKER)
    expect(backing.dump()).toEqual({})
    // and the page still reads what it wrote while it is open
    expect(store.getItem('genoffice.sheets.findHistory')).toBe(JSON.stringify([MARKER]))
    expect(store.getItem('aidocs.list.recent.bullets')).toBe(MARKER)
  })

  it('never reads a value an earlier page left under a key it keeps in memory', () => {
    const backing = backingStore({
      'ai-excel-chat-history': JSON.stringify([{ role: 'user', content: MARKER }]),
      'genoffice-pdf-view-state': MARKER,
    })
    const store = createEmbedStorage(backing)
    expect(store.getItem('ai-excel-chat-history')).toBeNull()
    expect(store.getItem('genoffice-pdf-view-state')).toBeNull()
  })

  it('persists the allowed layout keys', () => {
    const backing = backingStore()
    const store = createEmbedStorage(backing)
    store.setItem('ai-slides-thumbs-width', '212')
    expect(backing.dump()).toEqual({ 'ai-slides-thumbs-width': '212' })
    expect(store.getItem('ai-slides-thumbs-width')).toBe('212')
    store.removeItem('ai-slides-thumbs-width')
    expect(backing.dump()).toEqual({})
  })

  it('lists what it holds across both, and clears only its own keys', () => {
    const backing = backingStore({ 'unrelated-origin-key': 'keep', 'docs-ai-panel-width': '320' })
    const store = createEmbedStorage(backing)
    store.setItem('x', '1')
    expect(store.length).toBe(2)
    expect([store.key(0), store.key(1)].sort()).toEqual(['docs-ai-panel-width', 'x'])
    store.clear()
    expect(store.length).toBe(0)
    expect(store.getItem('x')).toBeNull()
    expect(backing.dump()).toEqual({ 'unrelated-origin-key': 'keep' })
  })

  it('works with no storage behind it: a blocked origin', () => {
    const store = createEmbedStorage(null)
    store.setItem('ai-slides-thumbs-width', '200')
    expect(store.getItem('ai-slides-thumbs-width')).toBe('200')
  })

  it('allows no key that names a document, a path or a file', () => {
    for (const key of PERSISTED_KEYS)
      expect(key).not.toMatch(/history|recent|shows|guides|view-state|dictionary|job-pages/)
  })
})

describe('what an older build left', () => {
  it('is removed at start, and only what holds document text, names or paths', () => {
    const backing = backingStore({
      'genoffice.sheets.findHistory': MARKER,
      'genoffice.sheets.replaceHistory': MARKER,
      'aisheets.spell-dictionary': MARKER,
      'ai-excel-chat-history': MARKER,
      'aidocs.list.recent.multi': MARKER,
      'genoffice-pdf-view-state': MARKER,
      'genoffice-pdf-job-pages:abc:ocr': MARKER,
      [`ai-slides-guides:/home/me/${MARKER}.pptx`]: MARKER,
      [`ai-slides-custom-shows:/home/me/${MARKER}.pptx`]: MARKER,
      'docs-ai-panel-width': '320',
      'other-app-key': 'keep',
    })
    purgeLegacyDocumentKeys(backing)
    expect(backing.dump()).toEqual({ 'docs-ai-panel-width': '320', 'other-app-key': 'keep' })
  })
})

describe('installing it on a page', () => {
  const real = Object.getOwnPropertyDescriptor(window, 'localStorage')
  afterEach(() => {
    // jsdom defines localStorage on the window itself: put the original back
    if (real) Object.defineProperty(window, 'localStorage', real)
    else Reflect.deleteProperty(window, 'localStorage')
    localStorage.clear()
  })

  it('swaps window.localStorage: what the page writes never reaches the real one', () => {
    const origin = window.localStorage
    origin.setItem('aisheets.spell-dictionary', MARKER)
    installEmbedStorage()
    expect(window.localStorage).not.toBe(origin)
    // the old value is cleared at start and is not read
    expect(origin.getItem('aisheets.spell-dictionary')).toBeNull()
    window.localStorage.setItem('genoffice.sheets.findHistory', MARKER)
    window.localStorage.setItem('ai-sheets-ribbon-collapsed', '1')
    expect(origin.getItem('genoffice.sheets.findHistory')).toBeNull()
    expect(origin.getItem('ai-sheets-ribbon-collapsed')).toBe('1')
    expect(JSON.stringify({ ...origin })).not.toContain(MARKER)
  })

  it('does nothing in a worker, which has no window', () => {
    expect(() => installEmbedStorage(undefined)).not.toThrow()
  })
})
