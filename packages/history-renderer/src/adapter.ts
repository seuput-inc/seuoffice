import type { CollabAdapter } from '@seuoffice/collab/client'

export type ByteHistoryAdapter<Op> = Pick<
  CollabAdapter<Op>,
  'decode' | 'encode' | 'integrate' | 'captureUndo' | 'produceFile'
> & { dispose?(): void }
