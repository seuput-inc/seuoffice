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
import { openHwpHistory } from './hwp'
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
    const officeArchive = name === 'docx' || name === 'pptx' || name === 'xlsx'
    if (createHash('sha256').update(bytes).digest('hex') !== history.baseHash)
      throw new HistoryError('invalid_history', 'base file does not match its epoch')
    async function run<Op>(
      version: number,
      open: (b: Uint8Array) => Promise<ByteHistoryAdapter<Op>>,
    ) {
      if (schema !== version) throw new HistoryError('unsupported', 'unsupported history schema')
      if (officeArchive) await checkArchive(bytes)
      const adapter = await open(bytes).catch((error: unknown) => {
        // HWP refuses at version 0 by contract (its epochs are admitted that way). Elsewhere a
        // base outside the editor model is still the file of version 0; its first edit is
        // refused with the reason found here.
        if (!(error instanceof HistoryError) || name === 'hwp' || name === 'hwpx') throw error
        const refuse = (): never => {
          throw error
        }
        const unmodelled: ByteHistoryAdapter<Op> = {
          decode: refuse,
          encode: refuse,
          integrate: refuse,
          captureUndo: refuse,
          produceFile: async () => ({ fileName: `history.${name}`, bytes }),
        }
        return unmodelled
      })
      const result = await replay(adapter, bytes, history).finally(() => adapter.dispose?.())
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
      if (officeArchive && history.version > 0) await checkArchive(result.bytes)
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
      case 'hwp':
      case 'hwpx':
        return await run(1, (source) => openHwpHistory(name, source))
      default:
        throw new HistoryError('unsupported', 'unsupported document format')
    }
  } catch (error) {
    if (error instanceof HistoryError) throw error
    throw new HistoryError('invalid_history', 'could not open or render the supplied history')
  }
}
