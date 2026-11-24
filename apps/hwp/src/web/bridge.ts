import { DEFAULT_AI_PANEL_PREFS, NO_AUTO_SAVE_DEFAULT } from '@seuoffice/ui'
import {
  collabLink,
  createAiStreamBridge,
  createWebBridge,
  documentByPath,
  editMode,
  historyMode,
  installWebBridge,
  onHostSaveRequest,
  onHostThumbnailRequest,
  registerCollabCapable,
  reportDirty,
  reportError,
  reportOpened,
  saveReason,
  saveToHost,
  unsupported,
  waitForDocument,
  webAiSettings,
  webLanguage,
} from '@seuoffice/web-platform'
import type { WebDocument } from '@seuoffice/web-platform'
import { READ_ONLY_ATTRIBUTE } from '../../studio-plugin/read-only.mjs'
import { hwpFormatOfPath } from '../shared/ipc'
import type { CommitSaveResult, HwpApi, HwpFormat, SaveMode } from '../shared/ipc'
import { openHwpFromBytes } from './open-hwp'
import type { OpenedHwp } from './open-hwp'
import { newTabSpots, webSaveBytes } from './save-hwp'
import { watchSaveKey } from './save-key'

const noop = (): void => {}
const unsubscribe = (): (() => void) => noop

registerCollabCapable({ history: true })

/** `?mode=edit`: the host stores what this page saves */
const canSave = editMode()

let trackingDropped = false
let collabRequested = false
let collabBytes: Uint8Array | undefined
/** the document this page opened, and what its saves correct against */
let opened: { doc: WebDocument; hwp: OpenedHwp } | null = null
/** the format `beginSave` settled on; `newTabs` and `commitSave` need one */
let pendingFormat: HwpFormat | null = null
/** the renderer's save handlers; the editor's File menu, Ctrl+S and the host send to them */
const saveListeners = new Set<(mode: SaveMode, hostSaveId?: string) => void>()
/** the save a host request started, settled when that save ends */
let hostSave: { resolve: () => void; reject: (error: Error) => void } | null = null

function requestSave(mode: SaveMode, hostSaveId?: string): void {
  for (const listener of saveListeners) listener(mode, hostSaveId)
}

/** the end of a save, as the desktop main process's `settleSave` sees it */
function settleSave(ok: boolean, error = 'hwp: the document was not saved'): void {
  const waiter = hostSave
  hostSave = null
  if (!waiter) return
  if (ok) waiter.resolve()
  else waiter.reject(new Error(error))
}

/**
 * The save half of the desktop main process (hwp-main.ts `beginSave`, `newTabs`, `commitSave`)
 * for a page in edit mode: one target, the opened name in its own format, and the host in
 * place of the disk. Save As to another name or format is not offered.
 */
const saveApi: Partial<HwpApi> = {
  setDirty: (dirty) => {
    if (!collabRequested) reportDirty(dirty)
  },
  abortSave: () => {
    pendingFormat = null
    settleSave(false)
  },
  onSaveRequest: (listener) => {
    saveListeners.add(listener)
    return () => saveListeners.delete(listener)
  },
  requestFileCommand: (command) => {
    if (command === 'save') requestSave('save')
  },
  beginSave: async () => {
    pendingFormat = null
    const refuse = (error: string, reason?: 'tracked' | 'unverified') => {
      settleSave(false, error)
      return { ok: false as const, error, ...(reason ? { reason } : {}) }
    }
    if (collabRequested) return refuse('hwp: co-edited files can only be saved by the session')
    if (!opened) return refuse('hwp: no document is open')
    if (opened.hwp.tracked) return refuse('hwp: the document has tracked changes', 'tracked')
    // the one target a page has is the opened file itself
    if (opened.hwp.unverified) {
      return refuse('hwp: the structure of the opened file could not be checked', 'unverified')
    }
    const format = hwpFormatOfPath(opened.doc.fileName)
    if (!format) return refuse('hwp: no save format for this file name')
    pendingFormat = format
    return { ok: true, path: opened.doc.virtualPath, format }
  },
  newTabs: async (data, format) => (pendingFormat ? newTabSpots(data, format) : []),
  commitSave: async (data, tabs, options) => {
    const format = pendingFormat
    pendingFormat = null
    const done = (result: CommitSaveResult): CommitSaveResult => {
      if (result.ok) settleSave(true)
      else settleSave(false, result.error)
      return result
    }
    if (collabRequested) return done({ ok: false, error: 'hwp: a session save is required' })
    if (!format || !opened) return done({ ok: false, error: 'hwp: no save in progress' })
    const fixed = webSaveBytes(opened.hwp.base, format, data, tabs)
    if (!fixed.ok) return done(fixed)
    try {
      await saveToHost(
        opened.doc.fileName,
        fixed.bytes,
        saveReason(options?.hostSaveId, options?.auto === true),
      )
    } catch (error) {
      return done({ ok: false, error: error instanceof Error ? error.message : String(error) })
    }
    return done({
      ok: true,
      path: opened.doc.virtualPath,
      ...(fixed.tailsLost ? { tailsLost: true as const } : {}),
    })
  },
}

// AI turns go to the embed host, which calls its gateway (decision 0017)
const ai = createAiStreamBridge()

const hwpApi: Partial<HwpApi> = {
  consumePending: async (signal) => {
    const doc = await waitForDocument()
    if (!doc) return null
    signal?.throwIfAborted()
    collabRequested = historyMode() || collabLink() !== null
    if (collabRequested) {
      // Ordinary pages must not load the collaboration WASM or source-certification readers.
      const { openHwpCollabPage } = await import('./collab-page').catch((error: unknown) => {
        if (!signal?.aborted)
          reportError('collab_unavailable', error instanceof Error ? error.message : String(error))
        throw error
      })
      collabBytes = await openHwpCollabPage(doc, signal)
    }
    return doc.virtualPath
  },
  untitledCopy: async () => null,
  readFile: async (path) => {
    const doc = documentByPath(path)
    if (!doc) throw new Error(`seuoffice web: no document at ${path}`)
    if (collabRequested) {
      if (!collabBytes || !window.seuHwpCollabPage) {
        throw new Error('hwp: no admitted collaboration document')
      }
      if (!historyMode()) queueMicrotask(reportOpened)
      return collabBytes
    }
    const hwp = openHwpFromBytes(doc, canSave)
    if (!hwp) throw new Error('seuoffice web: hwp refused')
    trackingDropped = hwp.trackingDropped
    opened = { doc, hwp }
    queueMicrotask(reportOpened)
    return hwp.bytes
  },
  trackingDropped: async () => trackingDropped,
  structureUnverified: async () => opened?.hwp.unverified ?? false,
  // one save target, the opened file: the editor's notices do not point at a Save As
  noSaveAs: true,
  getLanguage: async () => webLanguage(),
  onLanguageChanged: unsubscribe,
  getTheme: async () => 'system',
  onThemeChanged: unsubscribe,
  getAiPanelPrefs: async () => DEFAULT_AI_PANEL_PREFS,
  setAiPanelPrefs: async () => DEFAULT_AI_PANEL_PREFS,
  onAiPanelPrefsChanged: unsubscribe,
  getAutoSaveDefault: async () => NO_AUTO_SAVE_DEFAULT,
  onAutoSaveDefaultChanged: unsubscribe,
  getAiSettings: webAiSettings,
  onAiSettingsChanged: unsubscribe,
  getAuthorName: async () => ({ value: '', fallback: '' }),
  getMemoModalOpen: async () => false,
  setMemoModalOpen: noop,
  setDirty: noop,
  abortSave: noop,
  sendCloseSaveResult: noop,
  sendReadBytesResult: noop,
  sendRecoveryFlushed: noop,
  requestFileCommand: noop,
  headlessExportDone: noop,
  consumeHeadlessExport: async () => null,
  recoveryBegin: async () => ({ ok: false }),
  recoveryOpened: async () => ({ restored: false, notice: null }),
  getPathForFile: () => '',
  openDroppedDocuments: () => false,
  aiToolCapabilities: async () => null,
  onThumbnailRequest: onHostThumbnailRequest,
  // menu and shell requests that never arrive on the web
  onSaveRequest: unsubscribe,
  onPrintRequest: unsubscribe,
  onPreviewRequest: unsubscribe,
  onCloseSaveRequest: unsubscribe,
  onMemoBlocked: unsubscribe,
  onReadBytesRequest: unsubscribe,
  onRecoveryFlush: unsubscribe,
  onRecoveryReset: unsubscribe,
  beginSave: unsupported('hwp.save'),
  commitSave: unsupported('hwp.save'),
  newTabs: unsupported('hwp.save'),
  recoveryNewTabs: unsupported('hwp.save'),
  recoveryWrite: unsupported('hwp.save'),
  printPages: unsupported('hwp.print'),
  aiStream: ai.aiStream,
  aiStreamCancel: ai.aiStreamCancel,
  onAiStream: ai.onAiStream,
  // desktop-only services: undefined turns their features off, as without a shell
  aiSubscription: undefined,
  pickAttachments: undefined,
  addAttachmentPaths: undefined,
  addPastedImage: undefined,
  readAttachment: undefined,
  readAttachmentImage: undefined,
  readAttachmentDocument: undefined,
  analyzeMedia: undefined,
  openAiModelSettings: undefined,
  readUrl: undefined,
  imageSearch: undefined,
  generateImage: undefined,
  fetchImage: undefined,
  ...(canSave ? saveApi : {}),
}

/** As long as the desktop main process waits for a requested save (hwp-main.ts `requestHwpSave`) */
const HOST_SAVE_TIMEOUT_MS = 120_000

/** One save the host asked for: settles when that save ends, however late */
function saveForHost(ownerId: string): Promise<void> {
  if (saveListeners.size === 0) {
    return Promise.reject(new Error('hwp: the document is not ready to save'))
  }
  return new Promise<void>((resolve, reject) => {
    hostSave = { resolve, reject }
    // the host asks for the bytes, so the save runs even when nothing changed: the renderer
    // answers 'saveAs' without asking the editor, and here both modes write the opened name.
    // The owner id is handed to every call of that save (`CommitSaveOptions.hostSaveId`).
    requestSave('saveAs', ownerId)
  })
}

export function installBridges(): void {
  installWebBridge('hwpApi', createWebBridge<HwpApi>('hwpApi', hwpApi))
  if (!canSave) {
    // a page the host does not store saves from shows the document and cannot change it;
    // set before the renderer makes the editor's frame, which reads it once at start-up
    document.documentElement.setAttribute(READ_ONLY_ATTRIBUTE, '')
    return
  }
  watchSaveKey(() => requestSave('save'))
  // One host request at a time, in arrival order. The next one starts only once the save before
  // it has really ended, even when its host stopped waiting: a late end would settle the next one.
  let previous: Promise<unknown> = Promise.resolve()
  onHostSaveRequest((ownerId) => {
    let refused = false
    // a request refused while it still waited for its turn starts no save
    const saved = previous.then(() => (refused ? undefined : saveForHost(ownerId)))
    previous = saved.catch(() => {})
    return new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        refused = true
        reject(new Error('hwp: the save did not finish in time'))
      }, HOST_SAVE_TIMEOUT_MS)
      saved.then(
        () => {
          clearTimeout(timer)
          resolve()
        },
        (error: unknown) => {
          clearTimeout(timer)
          reject(error)
        },
      )
    })
  })
}
