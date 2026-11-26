/**
 * @vitest-environment jsdom
 */
import { afterEach, describe, expect, it } from 'vitest'
import { markWebHost, webShowOnly } from '../src/web-host-mode'

const root = document.documentElement

function open(query: string): void {
  history.replaceState(null, '', `/sheets/index.html${query}`)
}

afterEach(() => {
  for (const name of [...root.getAttributeNames()]) {
    if (name.startsWith('data-web-')) root.removeAttribute(name)
  }
  open('')
})

describe('a page that only shows its document', () => {
  it('is a web view page nobody else edits', () => {
    open('?mode=view')
    markWebHost({ ai: false, autoSave: false })
    expect(webShowOnly()).toBe(true)
  })

  it('is not a view page that co-edits or replays a history', () => {
    open('?mode=view')
    markWebHost({ ai: false, autoSave: false, shared: true })
    expect(webShowOnly()).toBe(false)
  })

  it('is not an edit page', () => {
    open('?mode=edit')
    markWebHost({ ai: false, autoSave: false })
    expect(webShowOnly()).toBe(false)
  })

  it('is not a desktop app', () => {
    open('?mode=view')
    expect(webShowOnly()).toBe(false)
  })
})
