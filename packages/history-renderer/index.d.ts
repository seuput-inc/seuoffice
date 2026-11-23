import type { HistoryResult, Json } from '@seuoffice/collab/client'

export declare function renderBytes(
  baseBytes: Uint8Array,
  requestJson: Uint8Array,
): Promise<HistoryResult>

/** Only the caller may publish a successful stage. Its idempotent abort must discard pending writes. */
export declare function renderStream(
  source: { byteLength: number; open(): AsyncIterable<Uint8Array> },
  header: Uint8Array,
  changes: AsyncIterable<Uint8Array>,
  stage: { write(bytes: Uint8Array): Promise<void>; abort(): Promise<void> },
  signal?: AbortSignal,
): Promise<{ fileName: string; compensation?: Json[] }>

/** Mixed profiles share the authenticated server's stage. Undo requires the actual append author. */
export declare function renderHistory(
  ...args: [...Parameters<typeof renderStream>, compensationAuthor?: string]
): ReturnType<typeof renderStream>
