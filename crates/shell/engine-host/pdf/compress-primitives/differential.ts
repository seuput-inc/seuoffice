export interface CompressionCandidate {
  kind: 'jpeg' | 'raw'
  width: number
  height: number
  components: 1 | 3 | null
  dataBase64: string
  target: { longSide: number; quality: number }
  before: { width: number; height: number; bgraBase64: string } | null
  jpegBase64: string | null
  outputWidth: number | null
  outputHeight: number | null
  decodedBgraBase64: string | null
}

export interface CompressionObservation {
  runtime: { engine: string; version: string; chromium: string; platform: string; arch: string }
  sourceBase64: string
  target: { longSide: number; quality: number }
  candidates: CompressionCandidate[]
  progress: Array<[number, number]>
  recoded: number
  outcome: 'written' | 'not-smaller'
  outputBase64: string | null
  sourceLength: number
  outputLength: number
  rendered: { width: number; height: number; bgraBase64: string } | null
}

export interface CompressionDifference {
  path: string
  expected: unknown
  actual: unknown
}

/** Runtime versions are recorded, not equated across Electron and CEF. */
export function compareCompression(expected: CompressionObservation, actual: CompressionObservation): CompressionDifference[] {
  const differences: CompressionDifference[] = []
  if (expected.runtime.engine !== 'Electron' || expected.runtime.version !== '43.8.3') {
    differences.push({ path: 'expected.runtime', expected: 'Electron 43.8.3', actual: expected.runtime })
  }
  if (actual.runtime.engine !== 'CEF') {
    differences.push({ path: 'actual.runtime', expected: 'CEF', actual: actual.runtime })
  }
  for (const [side, observation] of [['expected', expected], ['actual', actual]] as const) {
    if (!observation.runtime.version || !observation.runtime.chromium || observation.runtime.chromium === 'unknown' || !observation.sourceBase64 || !observation.candidates.length) {
      differences.push({ path: `${side}.evidence`, expected: 'runtime, source and candidate trace', actual: 'missing' })
    }
    for (const [index, candidate] of observation.candidates.entries()) {
      if (!candidate.dataBase64 || !candidate.before?.bgraBase64 || !candidate.jpegBase64 || !candidate.decodedBgraBase64 ||
          !candidate.outputWidth || !candidate.outputHeight) {
        differences.push({ path: `${side}.candidates[${index}].evidence`, expected: 'source, resized pixels, JPEG and decoded pixels', actual: 'missing' })
      }
    }
    if (observation.outcome === 'written' && (!observation.outputBase64 || !observation.rendered)) {
      differences.push({ path: `${side}.rendered`, expected: 'written PDF and PDFium rendering', actual: 'missing' })
    }
  }
  const compare = (path: string, left: unknown, right: unknown): void => {
    if (Object.is(left, right)) return
    if (Array.isArray(left) && Array.isArray(right)) {
      if (left.length !== right.length) differences.push({ path: `${path}.length`, expected: left.length, actual: right.length })
      for (let i = 0; i < Math.min(left.length, right.length); i++) compare(`${path}[${i}]`, left[i], right[i])
      return
    }
    if (left && right && typeof left === 'object' && typeof right === 'object' && !Array.isArray(left) && !Array.isArray(right)) {
      const a = left as Record<string, unknown>, b = right as Record<string, unknown>
      for (const key of new Set([...Object.keys(a), ...Object.keys(b)])) compare(`${path}.${key}`, a[key], b[key])
      return
    }
    if (typeof left === 'string' && typeof right === 'string' && (path.endsWith('Base64'))) {
      let index = 0
      while (index < left.length && index < right.length && left[index] === right[index]) index++
      differences.push({ path, expected: { length: left.length, firstMismatch: index }, actual: { length: right.length, firstMismatch: index } })
      return
    }
    differences.push({ path, expected: left, actual: right })
  }
  for (const key of ['sourceBase64', 'target', 'candidates', 'progress', 'recoded', 'outcome', 'outputBase64', 'sourceLength', 'outputLength', 'rendered'] as const) {
    compare(key, expected[key], actual[key])
  }
  return differences
}
