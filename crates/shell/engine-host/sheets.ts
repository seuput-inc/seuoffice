import { z } from 'zod'
import {
  externalLinksResultSchema,
  externalLinksUpdateResultSchema,
  saveEditsChunkArraySchema,
  workbookExportPdfRequestSchema,
  workbookConnectionsResultSchema,
  workbookFileSchema,
  workbookFindCellsResultSchema,
  workbookFormulaCellsResultSchema,
  workbookMediaResultSchema,
  workbookPivotDefinitionSchema,
  workbookQueriesResultSchema,
  workbookRangeResultSchema,
  workbookRecalcResultSchema,
  workbookRedactionsResultSchema,
  workbookRefreshResultSchema,
  workbookRowOutlineResultSchema,
  workbookSaveRequestSchema,
  workbookSlicerStateResultSchema,
} from '../../../apps/sheets/src/shared/desktop-api'
import { decode, encode } from '../bridge/wire'

const archiveEntry = z.object({
  name: z.string(),
  crc32: z.number(),
  compressedSize: z.number(),
  uncompressedSize: z.number(),
})

const schemas = {
  archiveNames: z.object({ entries: z.array(z.object({ name: z.string() })) }),
  archiveManifest: z.object({ entries: z.array(archiveEntry) }),
  archiveReadEntries: z.object({ entries: z.array(z.object({ name: z.string(), path: z.string() })) }),
  archiveScanEntries: z.object({ matches: z.array(z.string()) }),
  archiveSave: z.object({ beforeEntries: z.array(archiveEntry), afterEntries: z.array(archiveEntry) }),
  open: workbookFileSchema.omit({ sha256: true, readOnly: true }),
  range: workbookRangeResultSchema,
  formulaCells: workbookFormulaCellsResultSchema,
  findCells: workbookFindCellsResultSchema,
  rowOutline: workbookRowOutlineResultSchema,
  media: workbookMediaResultSchema,
  connections: workbookConnectionsResultSchema,
  pivot: workbookPivotDefinitionSchema,
  queries: workbookQueriesResultSchema,
  redactions: workbookRedactionsResultSchema,
  refresh: workbookRefreshResultSchema,
  slicers: workbookSlicerStateResultSchema,
  externalLinks: externalLinksResultSchema,
  externalLinksUpdate: externalLinksUpdateResultSchema,
  sidecarRecalc: z.object({
    cells: z.array(z.object({
      sheet: z.string(),
      row: z.number().int().nonnegative(),
      column: z.number().int().nonnegative(),
      formatted: z.string(),
      number: z.number().optional(),
      isError: z.boolean().optional(),
      isFormula: z.boolean(),
    }).strict()),
    cached: z.boolean().optional(),
  }).strict(),
  recalc: workbookRecalcResultSchema,
  save: workbookSaveRequestSchema,
  exportPdf: workbookExportPdfRequestSchema,
  editsChunk: saveEditsChunkArraySchema,
} as const

type SchemaName = keyof typeof schemas

export function parseWorkbookSchema(input: unknown): { header: unknown; buffers: string[] } {
  if (input === null || typeof input !== 'object' || !('schema' in input) || typeof input.schema !== 'string' ||
      !Object.hasOwn(schemas, input.schema) || !('header' in input) || !('buffers' in input) ||
      !Array.isArray(input.buffers) || !input.buffers.every((buffer: unknown) => typeof buffer === 'string')) {
    throw new TypeError('invalid workbook schema request')
  }
  const buffers = input.buffers.map((buffer: string) =>
    Uint8Array.from(atob(buffer), (character) => character.charCodeAt(0)))
  const value = schemas[input.schema as SchemaName].parse(decode(input.header, buffers))
  const result = encode(value)
  return {
    header: result.header,
    buffers: result.buffers.map((buffer) => {
      let binary = ''
      for (const byte of buffer) binary += String.fromCharCode(byte)
      return btoa(binary)
    }),
  }
}
