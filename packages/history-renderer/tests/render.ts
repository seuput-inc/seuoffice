import { renderBytes } from '@seuoffice/history-renderer'
import type { HistoryRequest } from '@seuoffice/collab/client'

export const render = (bytes: Uint8Array, request: HistoryRequest) =>
  renderBytes(bytes, new TextEncoder().encode(JSON.stringify(request)))
