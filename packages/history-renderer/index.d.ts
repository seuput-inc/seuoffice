import type { HistoryResult, Json } from '@seuoffice/collab/client'

export declare function renderBytes(
  baseBytes: Uint8Array,
  requestJson: Uint8Array,
): Promise<HistoryResult>

/** Write only to private staging; the host publishes atomically after this promise succeeds. */
export declare function renderStream(
  source: { byteLength: number; open(): AsyncIterable<Uint8Array> },
  header: Uint8Array,
  changes: AsyncIterable<Uint8Array>,
  stage: { write(bytes: Uint8Array): Promise<void>; abort(): Promise<void> },
): Promise<{ fileName: string; compensation?: Json[] }>
