/**
 * What an embedded page keeps in the SeuOffice origin's localStorage. The origin is shared by
 * every document any host opens in it, often on a shared PC, so a page must leave nothing of a
 * document behind: no search or replace text, no list formats, no custom show names, no file
 * names, no learned words. The desktop apps keep all of those in localStorage; the web build
 * swaps `localStorage` for a store that persists only the keys below and holds every other
 * key in memory, for as long as the page is open.
 *
 * A key is persisted only when its value is a document-independent layout preference (a width,
 * a collapsed flag, a unit). Everything else, including a key added tomorrow, stays in memory by
 * default; a new persisted key is a deliberate entry here and a row in packages/embed/README.md.
 */

export const PERSISTED_KEYS: ReadonlySet<string> = new Set([
  // ribbon collapsed or expanded
  'aidocs.ribbonCollapsed',
  'ai-sheets-ribbon-collapsed',
  'ai-slides-ribbon-collapsed',
  'genoffice-pdf-ribbon-collapsed',
  // panel and pane widths, in px
  'docs-ai-panel-width',
  'sheets-ai-panel-width',
  'pdf-ai-panel-width',
  'hwp-ai-panel-width',
  'aidocs.navWidth',
  'ai-slides-thumbs-width',
  'ai-slides-presenter-side-width',
  'genoffice-pdf-sidebar-width',
  'genoffice-pdf-bilingual-width',
  'hwpapp.memoPanelWidth',
  // which side pane is open, and the pdf sidebar's open flag
  'aidocs.leftPane',
  'genoffice-pdf-sidebar-open',
  // the measurement unit the person picked
  'aidocs.measurementUnit',
  // the hwp app seeds the studio's settings (theme, recovery drafts off) before its frame exists,
  // and the frame reads them from the real storage: it holds no document text
  'rhwp-settings',
])

/**
 * Keys earlier builds wrote from a document: its text, its file name or its path. A page removes
 * them when it starts, so a PC that ran an older build is clean after the first load.
 */
const LEGACY_DOCUMENT_KEYS: readonly string[] = [
  // sheets: find and replace history, learned spelling words, the chat history older builds kept
  'genoffice.sheets.findHistory',
  'genoffice.sheets.replaceHistory',
  'aisheets.spell-dictionary',
  'ai-excel-chat-history',
  // docs: recently used list formats (they hold the person's bullet and number text)
  'aidocs.list.recent.bullets',
  'aidocs.list.recent.numbers',
  'aidocs.list.recent.multi',
  // pdf: view state keyed by file path
  'genoffice-pdf-view-state',
]

const LEGACY_DOCUMENT_PREFIXES: readonly string[] = [
  // slides: custom shows and guides, keyed by file path
  'ai-slides-custom-shows:',
  'ai-slides-guides:',
  // pdf: pages of an AI job, keyed by a hash of the file
  'genoffice-pdf-job-pages:',
  // the hwp studio: font preferences keyed by the digest of the document
  'rhwp-document-font-preferences',
]

/** Remove what an older build left from a document; storage that refuses removal is left alone */
export function purgeLegacyDocumentKeys(backing: Storage): void {
  try {
    const stale: string[] = []
    for (let i = 0; i < backing.length; i++) {
      const key = backing.key(i)
      if (
        key !== null &&
        (LEGACY_DOCUMENT_KEYS.includes(key) ||
          LEGACY_DOCUMENT_PREFIXES.some((prefix) => key.startsWith(prefix)))
      ) {
        stale.push(key)
      }
    }
    for (const key of stale) backing.removeItem(key)
  } catch {
    /* storage blocked: nothing was written there either */
  }
}

/**
 * A `Storage` for an embedded page: persisted keys go to `backing`, every other key lives in
 * memory and is gone with the page. Reads follow the same split, so a value an earlier page
 * wrote under a memory-only key is never read.
 */
export function createEmbedStorage(
  backing: Storage | null,
  persisted: ReadonlySet<string> = PERSISTED_KEYS,
): Storage {
  const memory = new Map<string, string>()
  const keys = (): string[] => {
    const all = new Set(memory.keys())
    try {
      for (const key of persisted) if (backing?.getItem(key) != null) all.add(key)
    } catch {
      /* storage blocked: only memory */
    }
    return [...all]
  }
  const store: Storage = {
    get length() {
      return keys().length
    },
    key: (index) => keys()[index] ?? null,
    getItem(key) {
      const name = String(key)
      if (persisted.has(name)) {
        try {
          const stored = backing?.getItem(name)
          if (stored != null) return stored
        } catch {
          /* storage blocked: what the page wrote is in memory */
        }
      }
      return memory.get(name) ?? null
    },
    setItem(key, value) {
      const name = String(key)
      if (!persisted.has(name)) {
        memory.set(name, String(value))
        return
      }
      try {
        if (!backing) throw new Error('no storage')
        backing.setItem(name, String(value))
      } catch {
        memory.set(name, String(value))
      }
    },
    removeItem(key) {
      memory.delete(String(key))
      if (!persisted.has(key)) return
      try {
        backing?.removeItem(key)
      } catch {
        /* storage blocked */
      }
    },
    clear() {
      memory.clear()
      try {
        for (const key of persisted) backing?.removeItem(key)
      } catch {
        /* storage blocked */
      }
    },
  }
  return store
}

/**
 * Replace the page's `localStorage` with the embed store and clear what older builds left. A
 * worker has no localStorage (and `scope` is undefined there): nothing to do.
 */
export function installEmbedStorage(scope: Window | undefined = globalThis.window): void {
  if (!scope) return
  let backing: Storage | null = null
  try {
    backing = scope.localStorage
  } catch {
    // storage blocked outright: the page keeps everything in memory
  }
  if (backing) purgeLegacyDocumentKeys(backing)
  Object.defineProperty(scope, 'localStorage', {
    configurable: true,
    value: createEmbedStorage(backing),
  })
}
