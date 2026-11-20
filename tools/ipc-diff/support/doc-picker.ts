/**
 * What the doc-picker group loads `apps/shell/src/main/doc-picker` with. The
 * hosts it reaches are stood in for as the Rust probe stands in for them
 * (`crates/shell-ipc/w/src/doc_picker/probe.rs`), from the case's
 * `hosts.w.docPicker`:
 *
 * - `view`: the view the picker window opens as (2 by default); `lang`,
 *   `theme`: what the app answers (`ko`, `light`);
 * - `open`, `recent`, `project`, `sizes`: what the list is built from
 *   (`{ id, path?, title, dirty }`, `{ path, title, missing }`,
 *   `{ path, title }` or `{ throws: true }`, a size by path);
 * - `askers`: the asker of each view, by view number;
 * - `asked`: `{ parent, asker }`, an editor in window `parent` asked for the
 *   picker before the case starts; its answer is logged when it comes;
 * - `userCloses`: the person closes each picker window as soon as it is
 *   created, before its page has said `doc-picker:ready` (so it is never shown).
 *
 * `new BrowserWindow` is a window whose calls are log lines named as the
 * Electron call. The module shows it only when the picker page invokes
 * `doc-picker:ready` with the window's token (`readyToken` of `doc-picker:get-init`).
 */
import * as attachable from '../../../packages/file-parse/src/attachable.ts'
import type { LoadContext } from '../types.ts'

interface Asker {
  tabId?: string
  path?: string
  paneTabId?: string
}

export interface DocPickerSpec {
  view?: number
  lang?: 'ko' | 'en'
  theme?: string
  open?: { id: string; path?: string; title: string; dirty: boolean }[]
  recent?: { path: string; title: string; missing: boolean }[]
  project?: { path: string; title: string }[] | { throws: true }
  sizes?: Record<string, number>
  askers?: Record<string, Asker>
  asked?: { parent: number; asker: Asker }
  userCloses?: boolean
}

export const specOf = (cx: LoadContext): DocPickerSpec =>
  ((cx.case.hosts?.w ?? {}) as { docPicker?: DocPickerSpec }).docPicker ?? {}

const WINDOW_KEYS = [
  'width',
  'height',
  'minWidth',
  'minHeight',
  'parent',
  'modal',
  'show',
  'center',
  'minimizable',
  'maximizable',
  'fullscreenable',
  'autoHideMenuBar',
  'title',
]

/** The `DocPickerDeps` of this case, and the `BrowserWindow` it opens its window with */
export function standIn(cx: LoadContext) {
  const spec = specOf(cx)
  const log = cx.electron.log
  const contents = cx.electron.contents(spec.view ?? 2) as Record<string, unknown> | undefined
  const statics = cx.electron.BrowserWindow as Record<string, unknown>

  class FakeBrowserWindow {
    static getAllWindows = statics.getAllWindows
    static fromWebContents = statics.fromWebContents
    static fromId = statics.fromId
    static getFocusedWindow = statics.getFocusedWindow

    readonly webContents = contents
    private destroyed = false
    private readonly closed: (() => void)[] = []

    constructor(options: Record<string, unknown>) {
      const parent = options.parent as { id: number } | undefined
      log.push([
        'new BrowserWindow',
        Object.fromEntries(
          WINDOW_KEYS.filter((k) => k in options).map((k) => [
            k,
            k === 'parent' ? parent?.id : options[k],
          ]),
        ),
      ])
      if (contents) contents.setWindowOpenHandler = () => {}
    }
    removeMenu(): void {}
    on(event: string, listener: () => void): void {
      if (event === 'closed') this.closed.push(listener)
    }
    loadFile(): void {
      this.pageLoads()
    }
    loadURL(): void {
      this.pageLoads()
    }
    /** the module loads the page last, after it listens for `closed`: a person closes it here */
    private pageLoads(): void {
      if (spec.userCloses) setImmediate(() => this.end())
    }
    isDestroyed(): boolean {
      return this.destroyed
    }
    show(): void {
      log.push(['BrowserWindow.show'])
    }
    focus(): void {
      log.push(['BrowserWindow.focus'])
    }
    close(): void {
      log.push(['BrowserWindow.close'])
      setImmediate(() => this.end())
    }
    private end(): void {
      if (this.destroyed) return
      this.destroyed = true
      for (const listener of this.closed) listener()
    }
  }
  cx.install({ BrowserWindow: FakeBrowserWindow })

  const sizes = spec.sizes ?? {}
  const project = spec.project
  return {
    spec,
    deps: {
      listDeps: {
        openDocuments: async () => spec.open ?? [],
        recent: async () => spec.recent ?? [],
        ...(project === undefined
          ? {}
          : {
              projectFiles: async (asker: unknown) => {
                log.push(['deps.projectFiles', asker])
                if ('throws' in project) throw new Error('project failed')
                return project
              },
            }),
        stat: async (path: string) => {
          log.push(['deps.stat', path])
          return path in sizes ? { sizeBytes: sizes[path] } : null
        },
        attachable: {
          ATTACHABLE_EXTS: attachable.ATTACHABLE_EXTS,
          ATTACHABLE_IMAGE_EXTS: attachable.ATTACHABLE_IMAGE_EXTS,
          ATTACHABLE_MAX_BYTES: attachable.ATTACHABLE_MAX_BYTES,
          ATTACHABLE_IMAGE_MAX_BYTES: attachable.ATTACHABLE_IMAGE_MAX_BYTES,
          attachableMaxBytes: attachable.attachableMaxBytes,
          diagramPictureExtOfName: attachable.diagramPictureExtOfName,
        },
      },
      askerFor: (id: number): Asker => spec.askers?.[String(id)] ?? {},
      lang: () => spec.lang ?? 'ko',
      theme: () => (spec.theme ?? 'light') as 'light' | 'dark' | 'system',
    },
  }
}
