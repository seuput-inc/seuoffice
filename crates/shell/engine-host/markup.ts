import {
  detectDiagramFormat,
  formatDiagramText,
  openDiagram,
  type DiagramFormat,
  type DiagramOpenFailure,
  type PageCodec,
} from '../../../packages/drawio-model/src/index'
import { deflateSync, inflateSync } from 'fflate'
import { Buffer } from 'buffer'

export { HOST_STEPS as markupSteps } from '../../../packages/file-parse/src/host-steps'

const codec: PageCodec = {
  inflateRaw: (bytes) => inflateSync(bytes),
  deflateRaw: (bytes) => deflateSync(bytes),
}

function basename(path: string): string {
  const windows = navigator.userAgent.includes('Windows')
  const separator = (char: string): boolean => char === '/' || (windows && char === '\\')
  let end = path.length
  while (end > 0 && separator(path[end - 1]!)) end--
  let start = end
  while (start > 0 && !separator(path[start - 1]!)) start--
  return path.slice(start, end)
}

export function diagramFormatOf(filePath: string): DiagramFormat | null {
  const format = detectDiagramFormat(basename(filePath))
  return format === 'vsdx' ? null : format
}

export function readDiagramText(
  fileName: string,
  bytes: Uint8Array,
): { ok: true; text: string } | DiagramOpenFailure {
  const opened = openDiagram(basename(fileName), bytes, codec)
  if (!opened.ok) return opened
  return { ok: true, text: formatDiagramText(opened.file.state) }
}

export function openDiagramStep(input: unknown):
  | {
      ok: true
      xml: string
      format: DiagramFormat
      root: 'mxfile' | 'mxGraphModel'
      anyPageCompressed: boolean
    }
  | DiagramOpenFailure {
  if (
    input === null ||
    typeof input !== 'object' ||
    !('fileName' in input) ||
    typeof input.fileName !== 'string'
  ) {
    throw new TypeError('host step input has no string fileName')
  }
  if (!('bytesBase64' in input) || typeof input.bytesBase64 !== 'string') {
    throw new TypeError('host step input has no string bytesBase64')
  }
  const result = openDiagram(
    input.fileName,
    new Uint8Array(Buffer.from(input.bytesBase64, 'base64')),
    codec,
  )
  if (!result.ok) return result
  return {
    ok: true,
    xml: result.file.xml,
    format: result.file.format,
    root: result.file.root,
    anyPageCompressed: result.file.pages.some((page) => page.compressed),
  }
}
