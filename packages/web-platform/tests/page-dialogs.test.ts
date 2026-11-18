import { afterEach, describe, expect, it } from 'vitest'
import { pageDialogs } from '../src/page-dialogs'

const dialog = (): HTMLElement | null => document.querySelector('[data-web-sandbox-dialog]')
const press = (label: string): void =>
  [...dialog()!.querySelectorAll('button')].find((button) => button.textContent === label)!.click()
const key = (target: Element, name: string): void =>
  void target.dispatchEvent(new KeyboardEvent('keydown', { key: name, bubbles: true }))

afterEach(() => document.body.replaceChildren())

describe('pageDialogs.confirm', () => {
  it('answers true for OK and false for Cancel, and closes', async () => {
    const yes = pageDialogs.confirm('Link to the previous section?')
    expect(dialog()!.textContent).toContain('Link to the previous section?')
    press('확인')
    expect(await yes).toBe(true)
    expect(dialog()).toBeNull()

    const no = pageDialogs.confirm('again')
    press('취소')
    expect(await no).toBe(false)
  })

  it('answers false for Escape and gives the focus back', async () => {
    const before = document.createElement('button')
    document.body.append(before)
    before.focus()
    const answer = pageDialogs.confirm('sure?')
    expect(document.activeElement?.textContent).toBe('확인')
    key(document.activeElement!, 'Escape')
    expect(await answer).toBe(false)
    expect(document.activeElement).toBe(before)
  })

  it('shows markup in the message as text', async () => {
    const answer = pageDialogs.confirm('<img src=x onerror=alert(1)>')
    expect(dialog()!.querySelector('img')).toBeNull()
    press('취소')
    await answer
  })
})

describe('pageDialogs.prompt', () => {
  it('answers the entered text on Enter and null on Cancel', async () => {
    const named = pageDialogs.prompt('New name:', 'draft')
    const field = dialog()!.querySelector('input')!
    expect(field.value).toBe('draft')
    field.value = 'final'
    key(field, 'Enter')
    expect(await named).toBe('final')

    const cancelled = pageDialogs.prompt('New name:')
    press('취소')
    expect(await cancelled).toBeNull()
  })
})

describe('pageDialogs.alert', () => {
  it('settles once the person closes it', async () => {
    let settled = false
    const shown = pageDialogs.alert('No headings found').then(() => (settled = true))
    await Promise.resolve()
    expect(settled).toBe(false)
    press('확인')
    await shown
    expect(dialog()).toBeNull()
  })
})
