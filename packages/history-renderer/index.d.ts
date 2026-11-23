import type { HistoryResult } from '@seuoffice/collab/client'

export declare function renderBytes(
  baseBytes: Uint8Array,
  requestJson: Uint8Array,
): Promise<HistoryResult>
