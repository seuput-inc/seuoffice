// Starts the Rust document engine behind @seuoffice/docx-engine in this page, on first use: the
// equation dialog and metafile (EMF/WMF) pictures call it, and most workbooks need neither. The
// engine is a 27 MB wasm file a view-only page would otherwise fetch and compile before its
// first cell.
import { initDocxEngine } from '@seuoffice/docx-engine/init'
import engineUrl from '@seuoffice/docx-engine/engine-url'

let started: Promise<void> | undefined

/** Resolves once the engine runs; await it before the first call into @seuoffice/docx-engine */
export function startDocxEngine(): Promise<void> {
  started ??= initDocxEngine(engineUrl).catch((error: unknown) => {
    // a later use tries again: a dropped connection must not disable equations for the session
    started = undefined
    throw error
  })
  return started
}
