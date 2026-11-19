import { afterEach, describe, expect, it } from 'vitest'
import { SCHEME_FORM_GLOBAL } from '@seuoffice/electron-utils/custom-scheme-url'
import { DRAWIO_ORIGIN, drawioFrameOrigin, isFromDrawioFrame } from '../src/shared/bridge-protocol'
import { drawioStartUrl } from '../src/shared/drawio-start'

const holder = globalThis as Record<string, unknown>

afterEach(() => {
  delete holder[SCHEME_FORM_GLOBAL]
})

describe('the draw.io frame where WebView2 serves the scheme', () => {
  it('has the logical origin and address where the scheme is served as named', () => {
    expect(drawioFrameOrigin()).toBe(DRAWIO_ORIGIN)
    expect(
      drawioStartUrl({ lang: 'en', dark: false }).startsWith(`${DRAWIO_ORIGIN}/index.html?`),
    ).toBe(true)
  })

  it('is started at the editor under http://drawio-app.localhost', () => {
    holder[SCHEME_FORM_GLOBAL] = 'localhost-http'
    const url = new URL(drawioStartUrl({ lang: 'ko', dark: true }))
    expect(url.origin).toBe('http://drawio-app.localhost')
    expect(url.pathname).toBe('/editor/index.html')
    expect(url.searchParams.get('embed')).toBe('1')
    expect(url.searchParams.get('dark')).toBe('1')
  })

  it('reads messages only from the origin the frame really has there', () => {
    holder[SCHEME_FORM_GLOBAL] = 'localhost-http'
    const frame = {}
    expect(drawioFrameOrigin()).toBe('http://drawio-app.localhost')
    expect(
      isFromDrawioFrame({ source: frame, origin: 'http://drawio-app.localhost', data: '' }, frame),
    ).toBe(true)
    expect(isFromDrawioFrame({ source: frame, origin: DRAWIO_ORIGIN, data: '' }, frame)).toBe(false)
    expect(
      isFromDrawioFrame({ source: frame, origin: 'http://other.localhost', data: '' }, frame),
    ).toBe(false)
  })
})
