import type { HwpDocument } from '@rhwp/core'
import type { PresenceMark } from '@seuoffice/web-platform/presence'
import type { HwpPosition } from '../src/shared/collab-text'

/** Studio's caret slot on this exact version. Inline objects can occupy slots outside scalar text. */
export type HwpPresencePosition = Pick<HwpPosition, 'section' | 'paragraph' | 'path' | 'offset'>
export type HwpSelection = { anchor: HwpPresencePosition; head: HwpPresencePosition }

export interface HwpPresence {
  read(): HwpSelection | null
  draw(peers: readonly PresenceMark[]): void
  dispose(): void
}

/** Studio lends its document only for the duration of `read`. No editing authority is needed. */
export interface HwpPresenceHost {
  read<T>(fn: (doc: HwpDocument) => T): T
  events: { on(name: string, handler: () => void): () => void }
  automation: {
    registerCommand(command: { id: string; label: string; execute(services: unknown): void }): void
    execute(command: string): unknown
    unregisterCommand(command: string): void
  }
}

export declare function bindHwpPresence(host: HwpPresenceHost, publish: () => void): HwpPresence
