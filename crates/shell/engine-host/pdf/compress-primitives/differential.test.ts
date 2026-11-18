import { describe, expect, it } from 'vitest'
import { compareCompression, type CompressionObservation } from './differential'

function observation(engine: 'Electron' | 'CEF'): CompressionObservation {
  return {
    runtime: { engine, version: engine === 'Electron' ? '43.8.3' : '154.0.34', chromium: '1', platform: 'test', arch: 'test' },
    sourceBase64: 'AQ==', target: { longSide: 1400, quality: 60 },
    candidates: [{ kind: 'raw', width: 1, height: 1, components: 3, dataBase64: 'AQID',
      target: { longSide: 1400, quality: 60 }, before: { width: 1, height: 1, bgraBase64: 'AwIB/w==' },
      jpegBase64: '/9j/2Q==', outputWidth: 1, outputHeight: 1, decodedBgraBase64: 'AwIB/w==' }],
    progress: [[1, 1]], recoded: 1, outcome: 'written', outputBase64: 'JVBERg==',
    sourceLength: 3, outputLength: 2, rendered: { width: 1, height: 1, bgraBase64: 'AwIB/w==' },
  }
}

describe('compression differential evidence', () => {
  it('accepts only exact matching consumer and candidate evidence', () => {
    const expected = observation('Electron'), actual = observation('CEF')
    expect(compareCompression(expected, actual)).toEqual([])
    actual.candidates[0]!.jpegBase64 = '/9j/3Q=='
    actual.rendered!.bgraBase64 = 'BAIB/w=='
    actual.progress = [[1, 2]]
    const paths = compareCompression(expected, actual).map(difference => difference.path)
    expect(paths).toContain('candidates[0].jpegBase64')
    expect(paths).toContain('rendered.bgraBase64')
    expect(paths).toContain('progress[0][1]')
  })

  it('rejects matching but missing render and candidate evidence', () => {
    const a = observation('Electron'), b = observation('CEF')
    a.candidates = []; b.candidates = []
    a.rendered = null; b.rendered = null
    const paths = compareCompression(a, b).map(difference => difference.path)
    expect(paths).toContain('expected.evidence')
    expect(paths).toContain('actual.evidence')
    expect(paths).toContain('expected.rendered')
    expect(paths).toContain('actual.rendered')
  })
})
