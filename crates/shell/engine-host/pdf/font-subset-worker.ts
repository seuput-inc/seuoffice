interface HbSubset {
  memory: WebAssembly.Memory
  malloc(size: number): number
  free(ptr: number): void
  hb_blob_create(data: number, len: number, mode: number, userData: number, destroy: number): number
  hb_blob_destroy(blob: number): void
  hb_blob_get_data(blob: number, len: number): number
  hb_blob_get_length(blob: number): number
  hb_face_create(blob: number, index: number): number
  hb_face_destroy(face: number): void
  hb_face_reference_blob(face: number): number
  hb_set_add(set: number, value: number): void
  hb_set_clear(set: number): void
  hb_set_invert(set: number): void
  hb_subset_input_create_or_fail(): number
  hb_subset_input_destroy(input: number): void
  hb_subset_input_set(input: number, setType: number): number
  hb_subset_input_unicode_set(input: number): number
  hb_subset_or_fail(face: number, input: number): number
}

const worker = self as unknown as {
  location: Location
  onmessage: ((event: MessageEvent<unknown>) => void) | null
  postMessage(message: unknown): void
}

function decode(text: string): Uint8Array {
  const binary = atob(text)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  return bytes
}

function encode(bytes: Uint8Array): string {
  const chunks: string[] = []
  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    chunks.push(String.fromCharCode(...bytes.subarray(offset, offset + 0x8000)))
  }
  return btoa(chunks.join(''))
}

async function subset(font: Uint8Array, text: string): Promise<Uint8Array> {
  const response = await fetch(new URL('./hb-subset.wasm', worker.location.href))
  if (!response.ok) throw new Error(`Could not load HarfBuzz subset wasm (${response.status})`)
  const module = await WebAssembly.compile(await response.arrayBuffer())
  const instance = await WebAssembly.instantiate(module)
  const hb = instance.exports as unknown as HbSubset
  const input = hb.hb_subset_input_create_or_fail()
  if (!input) throw new Error('hb_subset_input_create_or_fail failed')
  let fontPtr = 0
  let blob = 0
  let face = 0
  let subsetFace = 0
  let result = 0
  try {
    fontPtr = hb.malloc(font.byteLength)
    if (!fontPtr && font.byteLength !== 0)
      throw new Error('HarfBuzz could not allocate font memory')
    new Uint8Array(hb.memory.buffer).set(font, fontPtr)
    blob = hb.hb_blob_create(fontPtr, font.byteLength, 2, 0, 0)
    face = hb.hb_face_create(blob, 0)
    hb.hb_blob_destroy(blob)
    blob = 0
    const features = hb.hb_subset_input_set(input, 6)
    hb.hb_set_clear(features)
    hb.hb_set_invert(features)
    const unicodes = hb.hb_subset_input_unicode_set(input)
    for (const ch of text) hb.hb_set_add(unicodes, ch.codePointAt(0)!)
    subsetFace = hb.hb_subset_or_fail(face, input)
    if (!subsetFace) throw new Error('hb_subset_or_fail failed (corrupt font?)')
    result = hb.hb_face_reference_blob(subsetFace)
    const len = hb.hb_blob_get_length(result)
    if (!len) throw new Error('hb-subset produced an empty font')
    const data = hb.hb_blob_get_data(result, 0)
    return new Uint8Array(hb.memory.buffer).slice(data, data + len)
  } finally {
    if (result) hb.hb_blob_destroy(result)
    if (subsetFace) hb.hb_face_destroy(subsetFace)
    if (face) hb.hb_face_destroy(face)
    if (blob) hb.hb_blob_destroy(blob)
    hb.hb_subset_input_destroy(input)
    if (fontPtr) hb.free(fontPtr)
  }
}

let started = false
worker.onmessage = async (event) => {
  if (started) return
  started = true
  try {
    const input = event.data
    if (
      !input ||
      typeof input !== 'object' ||
      !('fontBase64' in input) ||
      typeof input.fontBase64 !== 'string' ||
      !('text' in input) ||
      typeof input.text !== 'string'
    ) {
      throw new TypeError('invalid PDF font subset request')
    }
    const output = await subset(decode(input.fontBase64), input.text)
    worker.postMessage({ ok: encode(output) })
  } catch (thrown) {
    const error = thrown instanceof Error ? thrown : new Error(String(thrown))
    worker.postMessage({ error: { name: error.name, message: error.message } })
  }
}
