import { describe, expect, it } from 'vitest'
import { SCHEME_FORM_GLOBAL } from '../../../packages/electron-utils/src/custom-scheme-url'
import { IpcRenderer, type Transport } from './electron'
import { installSchemeForm, urlMappingFor } from './scheme-urls'
import { Encoder, decode, frame, unframe } from './wire'

const WINDOWS_PROBE = 'http://seuoffice-scheme-probe.localhost/'
const MAC_PROBE = 'seuoffice-scheme-probe://localhost/'

describe('the form of the page', () => {
  it('is fixed on the target from the probe, and cannot be changed by the page', () => {
    const target: Record<string, unknown> = {}
    installSchemeForm(target, WINDOWS_PROBE)
    expect(target[SCHEME_FORM_GLOBAL]).toBe('localhost-http')
    expect(Object.keys(target)).toEqual([])
    expect(() => {
      'use strict'
      target[SCHEME_FORM_GLOBAL] = 'scheme'
    }).toThrow()
  })

  it('maps nothing on macOS', () => {
    const target: Record<string, unknown> = {}
    expect(installSchemeForm(target, MAC_PROBE)).toBeUndefined()
    expect(target[SCHEME_FORM_GLOBAL]).toBe('scheme')
  })
})

describe('the IPC mapping on Windows', () => {
  const mapping = urlMapping()

  function urlMapping() {
    const found = urlMappingFor('localhost-http')
    if (!found) throw new Error('no mapping')
    return found
  }

  it('turns a URL the shell sent into the form WebView2 loads', () => {
    expect(mapping.toPage('epub-asset://tok/')).toBe('http://epub-asset.localhost/tok/')
    expect(mapping.toPage('seuoffice-docx-media://handoff/abc')).toBe(
      'http://seuoffice-docx-media.localhost/handoff/abc',
    )
    expect(mapping.toPage('html-preview://view-3/')).toBe('http://html-preview.localhost/view-3/')
  })

  it('turns a URL the page sends back into the one the shell knows', () => {
    expect(mapping.toShell('http://seuoffice-slide-media.localhost/archive/id?mime=a')).toBe(
      'seuoffice-slide-media://archive/id?mime=a',
    )
  })

  it('leaves everything else alone, md-asset included', () => {
    for (const text of [
      'plain',
      'md-asset:///C:/a.png',
      'http://md-asset.localhost/C:/a.png',
      'https://example.org/',
      'see epub-asset://tok/ inside a sentence',
    ]) {
      expect(mapping.toPage(text)).toBe(text)
      expect(mapping.toShell(text)).toBe(text)
    }
  })

  it('is absent where the scheme is served as named', () => {
    expect(urlMappingFor('scheme')).toBeUndefined()
  })
})

describe('IpcRenderer over a mapping', () => {
  function pair(withMapping: boolean) {
    const sent: Uint8Array[] = []
    let deliver: (bytes: Uint8Array) => void = () => {}
    const transport: Transport = {
      invoke: (bytes) => void sent.push(bytes),
      send: (bytes) => void sent.push(bytes),
      sendSync: () => {
        const e = new Encoder()
        return frame({ value: e.value('epub-asset://tok/') }, e.buffers)
      },
      listen: (onFrame) => {
        deliver = onFrame
      },
      droppedPaths: () => [],
    }
    const ipc = new IpcRenderer(
      transport,
      withMapping ? urlMappingFor('localhost-http') : undefined,
    )
    const reply = (head: Record<string, unknown>, value: unknown) => {
      const e = new Encoder()
      deliver(
        frame({ ...head, ...(value === undefined ? {} : { value: e.value(value) }) }, e.buffers),
      )
    }
    return { ipc, sent, reply, deliver: (bytes: Uint8Array) => deliver(bytes) }
  }

  function argsOf(bytes: Uint8Array): unknown {
    const { header, buffers } = unframe(bytes)
    return decode((header as { args: unknown }).args, buffers)
  }

  it('gives an invoke reply in the form of the page', async () => {
    const { ipc, reply } = pair(true)
    const answer = ipc.invoke('epub:open', 'a')
    reply({ reply: 1 }, { assetBase: 'epub-asset://tok/', title: 'A book' })
    expect(await answer).toEqual({ assetBase: 'http://epub-asset.localhost/tok/', title: 'A book' })
  })

  it('gives an event in the form of the page', () => {
    const { ipc, deliver } = pair(true)
    const seen: unknown[] = []
    ipc.on('html:preview-url', (_event, url) => seen.push(url))
    const e = new Encoder()
    const args = e.values(['html-preview://view-3/'])
    deliver(frame({ event: 'html:preview-url', args }, e.buffers))
    expect(seen).toEqual(['http://html-preview.localhost/view-3/'])
  })

  it('sends a URL in the logical form', () => {
    const { ipc, sent } = pair(true)
    ipc.send('slides:analyze', { mediaUrls: ['http://seuoffice-slide-media.localhost/archive/x'] })
    expect(argsOf(sent[0]!)).toEqual([{ mediaUrls: ['seuoffice-slide-media://archive/x'] }])
  })

  it('answers a sendSync in the form of the page', () => {
    expect(pair(true).ipc.sendSync('x')).toBe('http://epub-asset.localhost/tok/')
  })

  it('changes nothing without a mapping', async () => {
    const { ipc, reply, sent } = pair(false)
    const answer = ipc.invoke('epub:open')
    reply({ reply: 1 }, 'epub-asset://tok/')
    expect(await answer).toBe('epub-asset://tok/')
    ipc.send('x', 'http://epub-asset.localhost/tok/')
    expect(argsOf(sent[1]!)).toEqual(['http://epub-asset.localhost/tok/'])
    expect(pair(false).ipc.sendSync('x')).toBe('epub-asset://tok/')
  })
})
