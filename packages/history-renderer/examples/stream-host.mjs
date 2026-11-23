import { pipeline } from 'node:stream/promises'
import { createHistoryServer } from '@seuoffice/collab/server'
import { renderHistory } from '@seuoffice/history-renderer'
import { createStreamStore } from './stream-store.mjs'

// Supply a private DatabaseSync and synchronous policies that consult the host's current grants.
export function createStreamHost(database, authorize, authorizeTarget) {
  const host = createStreamStore(database, authorize, authorizeTarget)
  return {
    ...host,
    server: createHistoryServer({ store: host.store, renderStream: renderHistory }),
  }
}

// Only a published export may reach a response. Dispose it even if the client disconnects.
export async function downloadHistory(server, principal, point, response, signal) {
  const artifact = await server.exportHistory(principal, point, signal)
  try {
    await pipeline(artifact.open(), response, { signal })
  } finally {
    await artifact.dispose()
  }
}
