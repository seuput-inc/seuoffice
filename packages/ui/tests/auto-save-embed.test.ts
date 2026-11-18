/**
 * @vitest-environment jsdom
 */
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { act, createElement } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { useAutoSavePref } from '../src/auto-save-pref'
import { autoSaveOffered, markWebHost, webEmbedded } from '../src/web-host-mode'

const actEnvironment = globalThis as typeof globalThis & {
  IS_REACT_ACT_ENVIRONMENT?: boolean
}
actEnvironment.IS_REACT_ACT_ENVIRONMENT = true

const KEY = 'aidocs.autoSave'
const root = document.documentElement
const ATTRIBUTES = ['data-web-host', 'data-web-ai', 'data-web-autosave']

let host: HTMLDivElement
let mounted: Root

interface Harness {
  latest: [boolean, (on: boolean) => void] | null
}

function render(): Harness {
  host = document.createElement('div')
  document.body.append(host)
  mounted = createRoot(host)
  const harness: Harness = { latest: null }
  function Probe() {
    harness.latest = useAutoSavePref(KEY, undefined)
    return null
  }
  act(() => mounted.render(createElement(Probe)))
  return harness
}

beforeEach(() => localStorage.clear())
afterEach(() => {
  act(() => mounted.unmount())
  host.remove()
  for (const name of ATTRIBUTES) root.removeAttribute(name)
  localStorage.clear()
})

describe('autosave in a desktop app', () => {
  it('is offered, reads the stored choice and stores a new one', () => {
    localStorage.setItem(KEY, '1')
    const harness = render()
    expect(webEmbedded()).toBe(false)
    expect(autoSaveOffered()).toBe(true)
    expect(harness.latest![0]).toBe(true)
    act(() => harness.latest![1](false))
    expect(harness.latest![0]).toBe(false)
    expect(localStorage.getItem(KEY)).toBe('{"on":false,"base":0}')
  })
})

describe('autosave in a web embed whose host did not ask for it', () => {
  beforeEach(() => markWebHost({ ai: false, autoSave: false }))

  it('is not offered and stays off, whatever an earlier page stored', () => {
    localStorage.setItem(KEY, '{"on":true,"base":0}')
    const harness = render()
    expect(webEmbedded()).toBe(true)
    expect(autoSaveOffered()).toBe(false)
    expect(harness.latest![0]).toBe(false)
  })

  it('ignores a switch and writes nothing', () => {
    const harness = render()
    act(() => harness.latest![1](true))
    expect(harness.latest![0]).toBe(false)
    expect(localStorage.length).toBe(0)
  })
})

describe('autosave in a web embed whose host asked for it', () => {
  beforeEach(() => markWebHost({ ai: false, autoSave: true }))

  it('is offered and starts on, whatever an earlier page stored', () => {
    localStorage.setItem(KEY, '{"on":false,"base":0}')
    const harness = render()
    expect(autoSaveOffered()).toBe(true)
    expect(harness.latest![0]).toBe(true)
  })

  it('lets the person switch it off for the session without storing the choice', () => {
    const harness = render()
    act(() => harness.latest![1](false))
    expect(harness.latest![0]).toBe(false)
    act(() => harness.latest![1](true))
    expect(harness.latest![0]).toBe(true)
    expect(localStorage.length).toBe(0)
  })
})
