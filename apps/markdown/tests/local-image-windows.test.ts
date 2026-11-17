import { afterEach, describe, expect, it } from 'vitest'
import { SCHEME_FORM_GLOBAL } from '@seuoffice/electron-utils/custom-scheme-url'
import { resolveImageSrc, unresolveImageSrc } from '../src/renderer/editor/localImage'

const holder = globalThis as Record<string, unknown>

afterEach(() => {
  delete holder[SCHEME_FORM_GLOBAL]
})

describe('local images where WebView2 serves the scheme', () => {
  it('load from http://md-asset.localhost, the path being the file path', () => {
    holder[SCHEME_FORM_GLOBAL] = 'localhost-http'
    expect(resolveImageSrc('assets/图 1.png', 'C:\\notes')).toBe(
      'http://md-asset.localhost/C:/notes/assets/%E5%9B%BE%201.png',
    )
    expect(resolveImageSrc('/tmp/pic.png', null)).toBe('http://md-asset.localhost/tmp/pic.png')
  })

  it('come back to the authored path, and so do logical URLs a document may carry', () => {
    holder[SCHEME_FORM_GLOBAL] = 'localhost-http'
    const url = resolveImageSrc('assets/p.png', 'C:\\notes')
    expect(unresolveImageSrc(url, 'C:\\notes')).toBe('assets/p.png')
    expect(unresolveImageSrc('md-asset:///C:/notes/assets/p.png', 'C:\\notes')).toBe('assets/p.png')
    expect(unresolveImageSrc('http://md-asset.localhost/tmp/100%.png', '/docs')).toBe(
      '/tmp/100%.png',
    )
  })

  it('leave other addresses alone', () => {
    holder[SCHEME_FORM_GLOBAL] = 'localhost-http'
    expect(resolveImageSrc('https://x.test/a.png', '/docs')).toBe('https://x.test/a.png')
    expect(unresolveImageSrc('https://x.test/a.png', '/docs')).toBe('https://x.test/a.png')
  })

  it('stay logical where the scheme is served as named', () => {
    expect(resolveImageSrc('/tmp/pic.png', null)).toBe('md-asset:///tmp/pic.png')
    expect(unresolveImageSrc('md-asset:///tmp/pic.png', '/docs')).toBe('/tmp/pic.png')
  })
})
