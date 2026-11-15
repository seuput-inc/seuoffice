export interface RecodeRequest {
  kind: 'jpeg' | 'raw'
  data: Uint8Array
  width: number
  height: number
  components?: 1 | 3
  longSide: number
  quality: number
}

// Chromium's ImageOperations maps RESIZE_GOOD to Hamming-1. Coefficients
// and the two convolution passes use its 14-bit fixed-point representation.
function hammingFilters(
  source: number,
  destination: number,
): Array<{ start: number; weights: Int16Array }> {
  const f = Math.fround
  const scale = f(destination / source)
  const clamped = Math.min(1, scale)
  const inverse = f(1 / scale)
  const support = f(1 / clamped)
  const filters: Array<{ start: number; weights: Int16Array }> = []
  for (let target = 0; target < destination; target++) {
    const center = f(f(target + 0.5) * inverse)
    const start = Math.max(0, Math.floor(f(center - support)))
    const end = Math.min(source - 1, Math.ceil(f(center + support)))
    const values: number[] = []
    let sum = 0
    for (let pixel = start; pixel <= end; pixel++) {
      const distance = f(f(f(pixel + 0.5) - center) * clamped)
      const x = f(distance * f(Math.PI))
      const value =
        Math.abs(distance) >= 1
          ? 0
          : Math.abs(distance) < 1.1920928955078125e-7
            ? 1
            : f(f(f(Math.sin(x)) / x) * f(f(0.54) + f(f(0.46) * f(Math.cos(x)))))
      values.push(value)
      sum = f(sum + value)
    }
    const weights = Int16Array.from(values, (value) => Math.trunc(f(f(value / sum) * 16384)))
    let fixedSum = 0
    for (const weight of weights) fixedSum += weight
    weights[Math.floor(weights.length / 2)]! += 16384 - fixedSum
    filters.push({ start, weights })
  }
  return filters
}

export function resizeGood(
  source: Uint8ClampedArray,
  width: number,
  height: number,
  outWidth: number,
  outHeight: number,
): Uint8ClampedArray {
  const horizontal = hammingFilters(width, outWidth)
  const vertical = hammingFilters(height, outHeight)
  const result = new Uint8ClampedArray(outWidth * outHeight * 4)
  const clamp = (value: number) => Math.max(0, Math.min(255, value >> 14))
  const rows = new Map<number, Uint8ClampedArray>()
  const horizontalRow = (y: number): Uint8ClampedArray => {
    const existing = rows.get(y)
    if (existing) return existing
    const row = new Uint8ClampedArray(outWidth * 4)
    for (let x = 0; x < outWidth; x++) {
      const filter = horizontal[x]!
      let red = 0,
        green = 0,
        blue = 0
      for (let i = 0; i < filter.weights.length; i++) {
        const pos = (y * width + filter.start + i) * 4
        const weight = filter.weights[i]!
        red += weight * source[pos]!
        green += weight * source[pos + 1]!
        blue += weight * source[pos + 2]!
      }
      const dest = x * 4
      row[dest] = clamp(red)
      row[dest + 1] = clamp(green)
      row[dest + 2] = clamp(blue)
      row[dest + 3] = 255
    }
    rows.set(y, row)
    return row
  }
  for (let y = 0; y < outHeight; y++) {
    const filter = vertical[y]!
    for (const sourceY of rows.keys()) if (sourceY < filter.start) rows.delete(sourceY)
    const sourceRows = Array.from({ length: filter.weights.length }, (_, i) =>
      horizontalRow(filter.start + i),
    )
    for (let x = 0; x < outWidth; x++) {
      let red = 0,
        green = 0,
        blue = 0
      for (let i = 0; i < filter.weights.length; i++) {
        const row = sourceRows[i]!
        const pos = x * 4
        const weight = filter.weights[i]!
        red += weight * row[pos]!
        green += weight * row[pos + 1]!
        blue += weight * row[pos + 2]!
      }
      const dest = (y * outWidth + x) * 4
      result[dest] = clamp(red)
      result[dest + 1] = clamp(green)
      result[dest + 2] = clamp(blue)
      result[dest + 3] = 255
    }
  }
  return result
}

export async function recodeImage(
  request: RecodeRequest,
): Promise<{ jpeg: Uint8Array; width: number; height: number } | null> {
  const { width, height } = request
  if (!Number.isSafeInteger(width) || !Number.isSafeInteger(height) || width <= 0 || height <= 0)
    return null
  const source = new OffscreenCanvas(width, height)
  const context = source.getContext('2d')
  if (!context) throw new Error('Chromium image context unavailable')
  if (request.kind === 'jpeg') {
    let bitmap: ImageBitmap
    try {
      bitmap = await createImageBitmap(
        new Blob([new Uint8Array(request.data).buffer], { type: 'image/jpeg' }),
        { imageOrientation: 'none' },
      )
    } catch {
      return null
    }
    try {
      if (bitmap.width !== width || bitmap.height !== height) return null
      context.drawImage(bitmap, 0, 0)
    } finally {
      bitmap.close()
    }
  } else {
    const components = request.components
    if ((components !== 1 && components !== 3) || request.data.length < width * height * components)
      return null
    const pixels = context.createImageData(width, height)
    for (let i = 0; i < width * height; i++) {
      const src = i * components
      const dst = i * 4
      pixels.data[dst] = request.data[src]!
      pixels.data[dst + 1] = request.data[src + (components === 1 ? 0 : 1)]!
      pixels.data[dst + 2] = request.data[src + (components === 1 ? 0 : 2)]!
      pixels.data[dst + 3] = 255
    }
    context.putImageData(pixels, 0, 0)
  }
  const scale = Math.min(1, request.longSide / Math.max(width, height))
  const outWidth = Math.max(1, Math.round(width * scale))
  const outHeight = Math.max(1, Math.round(height * scale))
  let result = source
  if (scale < 1) {
    result = new OffscreenCanvas(outWidth, outHeight)
    const resized = result.getContext('2d')
    if (!resized) throw new Error('Chromium image context unavailable')
    const resizedPixels = resized.createImageData(outWidth, outHeight)
    resizedPixels.data.set(
      resizeGood(
        context.getImageData(0, 0, width, height).data,
        width,
        height,
        outWidth,
        outHeight,
      ),
    )
    resized.putImageData(resizedPixels, 0, 0)
  }
  const blob = await result.convertToBlob({ type: 'image/jpeg', quality: request.quality / 100 })
  return { jpeg: new Uint8Array(await blob.arrayBuffer()), width: outWidth, height: outHeight }
}
