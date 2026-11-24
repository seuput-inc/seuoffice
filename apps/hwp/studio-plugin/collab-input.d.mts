import type { HwpDocument } from '@rhwp/core'
import type { HwpPresence, HwpPresenceHost, HwpSelection } from './collab-presence.mjs'

export interface HwpStudioControls {
  setEditable(editable: boolean): void
  /** Synchronous borrow for trusted adapter reads and replay, never passed on to an RPC caller. */
  withDocument<T>(fn: (doc: HwpDocument) => T): T
  bindPresence(publish: () => void): HwpPresence
  /** Synchronous owner replay. Map Studio logical slots against the already updated model. */
  replay<T>(
    apply: (doc: HwpDocument, before: HwpSelection | null) => T,
    map: (before: HwpSelection) => HwpSelection | null,
  ): T
}

export interface HwpStudioCallbacks {
  transaction<T>(doc: HwpDocument, action: () => T): T
  onMutation(
    doc: HwpDocument,
    method: string,
    args: unknown[],
    call: (...replacement: unknown[]) => unknown,
  ): unknown
  onBuffer(buffered: boolean): void
  /** Recover locally only: this preedit never became a document change. */
  onDraftLost?(text: string): void
  /** A remote change removed the caret's place; input stays locked until the user clicks. */
  onCaretLost?(): void
}

declare global {
  interface Window {
    seuHwpCollabConnect?(frame: Window, controls: HwpStudioControls): HwpStudioCallbacks
  }
}

interface DocumentOwner {
  borrowDocumentHandle(): HwpDocument | null
}
interface StudioCollab {
  readonly locked: boolean
  readonly hasWrites: boolean
  transaction<T>(bridge: DocumentOwner, action: () => T): T
  confirmDialog(apply: () => void | boolean): void | boolean
  abortSnapshot(bridge: DocumentOwner, error: unknown): boolean
  deferEvent(
    bus: { emit(event: string, ...args: unknown[]): void },
    event: string,
    args: unknown[],
  ): boolean
  connect(bridge: DocumentOwner): void
  attachHost(host: HwpPresenceHost): void
  wrap(bridge: DocumentOwner, doc: HwpDocument): HwpDocument
  bindInput(input: unknown, commit: (text: string) => void): (() => void) | undefined
}
export declare function createStudioCollab(win?: Window): StudioCollab | null
export declare const studioCollab: StudioCollab | null
export declare function runStudioTransaction<T>(bridge: DocumentOwner, action: () => T): T
