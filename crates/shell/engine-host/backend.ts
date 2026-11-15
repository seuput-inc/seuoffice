import { pdfToText } from '../../../packages/file-parse/src/pdf'
import { xmindToText } from '../../../packages/file-parse/src/xmind'

export type ReaderName = 'reader.pdf' | 'reader.xmind'
export type ReaderResult = { text: string } | { error: { name: string; message: string } }

export async function readDocument(name: ReaderName, input: unknown): Promise<ReaderResult> {
  try {
    if (input === null || typeof input !== 'object' || !('bytesBase64' in input) || typeof input.bytesBase64 !== 'string') {
      throw new TypeError('reader input has no string bytesBase64')
    }
    const binary = atob(input.bytesBase64)
    const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0))
    switch (name) {
      case 'reader.pdf':
        return { text: await pdfToText(bytes) }
      case 'reader.xmind':
        return { text: await xmindToText(bytes) }
      default:
        throw new TypeError(`Unknown reader: ${String(name)}`)
    }
  } catch (error) {
    const thrown = error instanceof Error ? error : new Error(String(error))
    return { error: { name: thrown.name, message: thrown.message } }
  }
}
