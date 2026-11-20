import { openXmind } from '../../../packages/xmind-engine/src/open'

export async function openMindmap(input: unknown): Promise<{ status: string; detail?: string }> {
  if (
    input === null ||
    typeof input !== 'object' ||
    !('bytesBase64' in input) ||
    typeof input.bytesBase64 !== 'string'
  ) {
    throw new TypeError('mindmap input has no string bytesBase64')
  }
  const binary = atob(input.bytesBase64)
  const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0))
  const result = await openXmind(bytes)
  return result.status === 'ok'
    ? { status: result.status }
    : { status: result.status, detail: result.detail }
}
