/**
 * Node host using a private Playwright browser. The worker page loads the host's copy of
 * @seuoffice/embed as window.SeuofficeEmbed and its origin is on the office bundle's allowlist.
 * The two URLs, sessions and document ACLs come from server configuration, never an HTTP body.
 * This example keeps data in one process; supply a transactional RetainedHistoryStore for disk.
 *
 * const { head, changes } = await host.describe(token)
 * const fork = await host.history(token, {
 *   action: 'restore', expected: head, source: { epoch: head.epoch, version: 0 },
 *   requestId: crypto.randomUUID(),
 * })
 * By default choose one of the caller's changes and an index below its opCount. Reuse the exact
 * command and requestId after a lost reply. The host can set a narrower or wider target policy.
 */
import type { Browser } from 'playwright-core'
import type * as Embed from '@seuoffice/embed'
import {
  createHistoryServer,
  memoryHistoryStore,
  type HistoryPoint,
  type Principal,
  type RestoreRequest,
  type ServerSocket,
  type UndoRequest,
  type UndoTargetPolicy,
} from '@seuoffice/collab/server'

declare global {
  interface Window {
    SeuofficeEmbed: typeof Embed
  }
}

interface HostSession {
  documentId: string
  userId: string
  name: string
}

type Command =
  | { action: 'export'; source: HistoryPoint }
  | ({ action: 'restore' } & RestoreRequest)
  | ({ action: 'undo' } & UndoRequest)
  | { action: 'readRevision'; revisionId: string }

export function createDocxHistoryHost(options: {
  browser: Browser
  workerPageUrl: string
  officeBaseUrl: string
  /** Opaque, authenticated session tokens issued for one document. */
  sessions: ReadonlyMap<string, HostSession>
  grants: ReadonlyMap<string, ReadonlyMap<string, 'view' | 'edit'>>
  currentFileHashes: ReadonlyMap<string, string>
  authorizeUndoTarget?: UndoTargetPolicy
}) {
  const { browser, workerPageUrl, officeBaseUrl, sessions, grants } = options
  const store = memoryHistoryStore(({ userId, documentId }, action) => {
    const role = grants.get(documentId)?.get(userId)
    return role === 'edit' || (action === 'export' && role === 'view')
  }, options.authorizeUndoTarget)
  const server = createHistoryServer({
    store,
    async render(base, request) {
      if (request.format.name !== 'docx') throw new Error('not a DOCX history')
      const context = await browser.newContext()
      try {
        const page = await context.newPage()
        await page.goto(workerPageUrl)
        if (page.url() !== workerPageUrl) throw new Error('unexpected renderer page')
        return await page.evaluate(
          async ({ base, history, baseUrl }) => {
            const sdk = window.SeuofficeEmbed
            const fileName = 'history.docx'
            const file = await sdk.renderHistory(document.body, {
              baseUrl,
              fileName,
              bytes: base,
              history: JSON.parse(history),
            })
            if (file.fileName !== fileName) throw new Error('unexpected renderer file name')
            const check = sdk.openDocument(document.body, {
              mode: 'view',
              baseUrl,
              fileName,
              bytes: file.bytes,
            })
            try {
              await check.opened
            } finally {
              check.destroy()
            }
            return file
          },
          { base, history: JSON.stringify(request), baseUrl: officeBaseUrl },
        )
      } finally {
        await context.close()
      }
    },
  })

  function principal(token: string): Principal {
    const session = sessions.get(token)
    if (!session) throw new Error('not authenticated')
    const role = grants.get(session.documentId)?.get(session.userId)
    if (role !== 'view' && role !== 'edit') throw new Error('forbidden')
    return { ...session, canEdit: role === 'edit' }
  }

  return {
    accept(token: string, socket: ServerSocket) {
      const who = principal(token)
      const fileHash = options.currentFileHashes.get(who.documentId)
      if (!fileHash) throw new Error('no authoritative base file')
      server.accept(socket, { ...who, fileHash })
    },
    async describe(token: string) {
      const who = principal(token)
      await store.authorizeHistory(who, 'export')
      const epoch = await store.load(who.documentId)
      if (!epoch) throw new Error('no active epoch')
      const log = await store.read(who.documentId, epoch.id, 0, epoch.head)
      await store.authorizeHistory(who, 'export')
      return {
        head: { epoch: epoch.id, version: epoch.head },
        changes: log.map(({ v, author, ops }) => ({ v, author, opCount: ops.length })),
      }
    },
    /** Only the selector, expected head and retry key are taken from the request. */
    async history(token: string, command: Command) {
      const who = principal(token)
      switch (command.action) {
        case 'export':
          return server.exportHistory(who, command.source)
        case 'restore':
          return server.restoreHistory(who, command)
        case 'undo':
          return server.undoHistory(who, command)
        case 'readRevision':
          return store.readRevision(who, command.revisionId)
        default:
          throw new Error('unknown history command')
      }
    },
    close: server.close,
  }
}
