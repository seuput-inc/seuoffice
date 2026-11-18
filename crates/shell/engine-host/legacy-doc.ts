import { convertLegacyDocToDocx } from '../../../packages/doc2docx/src/docx-writer'

export interface LegacyDocResult {
  docx: string
  warnings: string[]
}

export async function convertLegacyDoc(input: unknown): Promise<LegacyDocResult> {
  if (
    input === null ||
    typeof input !== 'object' ||
    !('bytesBase64' in input) ||
    typeof input.bytesBase64 !== 'string'
  ) {
    throw new TypeError('legacy DOC input has no string bytesBase64')
  }
  const binary = atob(input.bytesBase64)
  const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0))
  const result = await convertLegacyDocToDocx(bytes)
  const chunks: string[] = []
  for (let offset = 0; offset < result.docx.length; offset += 0x8000) {
    chunks.push(String.fromCharCode(...result.docx.subarray(offset, offset + 0x8000)))
  }
  return { docx: btoa(chunks.join('')), warnings: result.warnings }
}
