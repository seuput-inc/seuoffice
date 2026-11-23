import { createHash } from 'node:crypto'
import {
  HistoryError,
  replayHistory,
  type HistoryRequest,
  type HistoryResult,
} from '@seuoffice/collab/client'
import type { ByteHistoryAdapter } from './adapter'
import { checkArchive } from './archive'
import { openDocx } from './docx'
import { openPdf } from './pdf'
import { openPptx } from './pptx'
import { checkBudget } from './request'
import { openXlsx } from './xlsx'

const MAX_FILE = 2 * 1024 * 1024

async function replay<Op>(
  adapter: ByteHistoryAdapter<Op>,
  bytes: Uint8Array,
  request: HistoryRequest,
) {
  return replayHistory(
    {
      ...adapter,
      // A byte adapter has no input device or local edits to subscribe to.
      listen: () => () => {},
      setEditable(on) {
        if (on) throw new HistoryError('unsupported', 'read-only renderer')
      },
      async fingerprint() {
        return createHash('sha256')
          .update((await adapter.produceFile()).bytes)
          .digest('hex')
      },
    },
    request.format,
    bytes,
    request,
  )
}

export async function render(
  bytes: Uint8Array,
  history: HistoryRequest,
  compensationAuthor?: string,
): Promise<HistoryResult> {
  try {
    if (!bytes.byteLength || bytes.byteLength > MAX_FILE) throw new HistoryError('invalid_history')
    checkBudget(history)
    const { name, schema } = history.format
    if (createHash('sha256').update(bytes).digest('hex') !== history.baseHash)
      throw new HistoryError('invalid_history', 'base file does not match its epoch')
    async function run<Op>(
      version: number,
      open: (b: Uint8Array) => Promise<ByteHistoryAdapter<Op>>,
    ) {
      if (schema !== version) throw new HistoryError('unsupported', 'unsupported history schema')
      if (name !== 'pdf') await checkArchive(bytes)
      const result = await replay(await open(bytes), bytes, history)
      if (result.bytes.byteLength > MAX_FILE)
        throw new HistoryError('unsupported', 'output exceeds 2 MiB')
      if (result.compensation) {
        const next = {
          v: history.version + 1,
          client: 'history:' + '0'.repeat(36),
          seq: 1,
          author: compensationAuthor ?? '',
          ops: result.compensation,
        }
        try {
          const prefix = { ...history, changes: [...history.changes, next] }
          if (compensationAuthor !== undefined) {
            delete prefix.undo
            prefix.version = next.v
          }
          checkBudget(prefix)
        } catch {
          throw new HistoryError('unsupported', 'compensated prefix exceeds replay limits')
        }
      }
      if (name !== 'pdf' && history.version > 0) await checkArchive(result.bytes)
      return result
    }
    switch (name) {
      case 'docx':
        return await run(4, openDocx)
      case 'xlsx':
        return await run(3, openXlsx)
      case 'pptx':
        return await run(3, openPptx)
      case 'pdf':
        return await run(2, openPdf)
      default:
        throw new HistoryError('unsupported', 'unsupported document format')
    }
  } catch (error) {
    if (error instanceof HistoryError) throw error
    throw new HistoryError('invalid_history', 'could not open or render the supplied history')
  }
}
