import { afterEach, describe, expect, it } from 'vitest'
import {
  CUSTOM_SCHEMES,
  SCHEME_FORM_GLOBAL,
  currentSchemeForm,
  isSchemeUrl,
  schemeCspSource,
  schemeFormOfProbe,
  toLogicalSchemeUrl,
  toPlatformSchemeUrl,
} from '../src/custom-scheme-url'

const WIN = 'localhost-http'

afterEach(() => {
  delete (globalThis as Record<string, unknown>)[SCHEME_FORM_GLOBAL]
})

describe('the form of a page', () => {
  it('is read from the probe of the shell preload', () => {
    expect(schemeFormOfProbe('http://seuoffice-scheme-probe.localhost/')).toBe(WIN)
    expect(schemeFormOfProbe('https://seuoffice-scheme-probe.localhost/')).toBe(WIN)
    expect(schemeFormOfProbe('seuoffice-scheme-probe://localhost/')).toBe('scheme')
  })

  it('is scheme unless the preload set the global', () => {
    expect(currentSchemeForm()).toBe('scheme')
    ;(globalThis as Record<string, unknown>)[SCHEME_FORM_GLOBAL] = WIN
    expect(currentSchemeForm()).toBe(WIN)
    ;(globalThis as Record<string, unknown>)[SCHEME_FORM_GLOBAL] = 'junk'
    expect(currentSchemeForm()).toBe('scheme')
  })
})

describe('toPlatformSchemeUrl', () => {
  it('leaves every URL as it is where the scheme is served as named', () => {
    for (const url of ['md-asset:///C:/a.png', 'epub-asset://tok/a.css', 'https://x.org/a'])
      expect(toPlatformSchemeUrl(url, 'scheme')).toBe(url)
  })

  it('moves the authority into the path on Windows, as native/url.rs does', () => {
    expect(toPlatformSchemeUrl('epub-asset://tok/OEBPS/a%20b.css?x=1#y', WIN)).toBe(
      'http://epub-asset.localhost/tok/OEBPS/a%20b.css?x=1#y',
    )
    expect(toPlatformSchemeUrl('epub-asset://tok/', WIN)).toBe('http://epub-asset.localhost/tok/')
    expect(toPlatformSchemeUrl('html-preview://view-3/', WIN)).toBe(
      'http://html-preview.localhost/view-3/',
    )
    expect(toPlatformSchemeUrl('drawio-app://editor/index.html?embed=1', WIN)).toBe(
      'http://drawio-app.localhost/editor/index.html?embed=1',
    )
    expect(toPlatformSchemeUrl('seuoffice-slide-media://archive/id-1?mime=image%2Fpng', WIN)).toBe(
      'http://seuoffice-slide-media.localhost/archive/id-1?mime=image%2Fpng',
    )
  })

  it('keeps an empty authority empty, so md-asset paths stay the path', () => {
    expect(toPlatformSchemeUrl('md-asset:///C:/docs/a%20b.png', WIN)).toBe(
      'http://md-asset.localhost/C:/docs/a%20b.png',
    )
    expect(toPlatformSchemeUrl('md-asset://', WIN)).toBe('http://md-asset.localhost')
  })

  it('leaves other schemes and platform URLs alone', () => {
    for (const url of [
      'https://example.org/a',
      'ftp://host/a',
      'data:image/png;base64,AAAA',
      'http://epub-asset.localhost/tok/a.css',
      'plain text',
    ])
      expect(toPlatformSchemeUrl(url, WIN)).toBe(url)
  })

  it('converts the schemes of the list and no others', () => {
    for (const scheme of CUSTOM_SCHEMES)
      expect(toPlatformSchemeUrl(`${scheme}://a/b`, WIN)).toBe(`http://${scheme}.localhost/a/b`)
  })

  it('reads the global when no form is given', () => {
    expect(toPlatformSchemeUrl('epub-asset://tok/a')).toBe('epub-asset://tok/a')
    ;(globalThis as Record<string, unknown>)[SCHEME_FORM_GLOBAL] = WIN
    expect(toPlatformSchemeUrl('epub-asset://tok/a')).toBe('http://epub-asset.localhost/tok/a')
  })
})

describe('toLogicalSchemeUrl', () => {
  it('is the inverse of toPlatformSchemeUrl for the schemes the shell hands out', () => {
    for (const logical of [
      'seuoffice-docx-media://handoff/abc',
      'seuoffice-docx-media://' + 'a'.repeat(64) + '/word/media/a%20b.png',
      'seuoffice-slide-media://archive/id?mime=image%2Fpng',
      'epub-asset://tok/OEBPS/a.css?x=1#y',
      'html-preview://view-3/',
      'html-asset://cap/_document',
      'drawio-app://editor/index.html',
    ]) {
      const platform = toPlatformSchemeUrl(logical, WIN)
      expect(platform).not.toBe(logical)
      expect(toLogicalSchemeUrl(platform, WIN)).toBe(logical)
    }
  })

  it('leaves logical URLs, md-asset URLs, other hosts and plain text as they are', () => {
    for (const url of [
      'epub-asset://tok/a',
      'http://md-asset.localhost/C:/a.png',
      'http://localhost/a',
      'http://tauri.localhost/a',
      'https://example.org/a',
      'text',
    ])
      expect(toLogicalSchemeUrl(url, WIN)).toBe(url)
  })

  it('does nothing where the scheme is served as named', () => {
    const url = 'http://epub-asset.localhost/tok/a'
    expect(toLogicalSchemeUrl(url, 'scheme')).toBe(url)
  })
})

describe('isSchemeUrl and schemeCspSource', () => {
  it('recognises a URL of a scheme in both forms', () => {
    expect(isSchemeUrl('seuoffice-slide-media://archive/x?mime=a', 'seuoffice-slide-media')).toBe(
      true,
    )
    expect(
      isSchemeUrl('http://seuoffice-slide-media.localhost/archive/x', 'seuoffice-slide-media'),
    ).toBe(true)
    expect(isSchemeUrl('http://other.localhost/archive/x', 'seuoffice-slide-media')).toBe(false)
    expect(isSchemeUrl('data:image/png;base64,AA', 'seuoffice-slide-media')).toBe(false)
  })

  it('names the CSP source of the form', () => {
    expect(schemeCspSource('epub-asset', 'scheme')).toBe('epub-asset:')
    expect(schemeCspSource('epub-asset', WIN)).toBe('http://epub-asset.localhost')
  })
})
