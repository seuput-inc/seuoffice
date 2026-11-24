import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { JSDOM } from 'jsdom'
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { createStudioCollab, type HwpStudioControls } from '../studio-plugin/collab-input.mjs'
import type { HwpPresenceHost } from '../studio-plugin/collab-presence.mjs'

beforeAll(async () => {
  const require = createRequire(import.meta.url)
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
})
const cleanup: (() => void)[] = []
afterEach(() => {
  while (cleanup.length) cleanup.pop()!()
  vi.unstubAllGlobals()
})

function page(mode = 'edit') {
  const parent = new JSDOM('<html data-studio-collab><body></body></html>', {
    url: `https://editor.test/app?mode=${mode}`,
    pretendToBeVisual: true,
  }).window
  const iframe = parent.document.createElement('iframe')
  iframe.src = 'https://editor.test/studio/'
  parent.document.body.append(iframe)
  const frame = iframe.contentWindow!
  const document = frame.document
  document.open()
  document.write(
    '<body><nav id="menu-bar"></nav><div id="scroll-container"><div id="scroll-content"></div></div><textarea></textarea><output></output></body>',
  )
  document.close()
  cleanup.push(() => parent.close())
  return { parent, frame, document }
}

type Pos = { sectionIndex: number; paragraphIndex: number; charOffset: number }
function editor(
  opts: {
    mode?: string
    readonly?: boolean
    ios?: boolean
    registered?: boolean
    nested?: boolean
  } = {},
) {
  const { frame, document, parent } = page(opts.mode)
  if (opts.readonly) parent.document.documentElement.setAttribute('data-studio-read-only', '')
  vi.stubGlobal('document', document)
  const order: (string | boolean)[] = []
  const batches: string[][] = []
  let staged: string[]
  const lost = vi.fn<(text: string) => void>()
  const caretLost = vi.fn()
  let controls!: HwpStudioControls
  if (opts.registered !== false) {
    const owner = parent as unknown as Window
    owner.seuHwpCollabConnect = (candidate, next) => {
      expect(candidate).toBe(frame)
      controls = next
      return {
        transaction(doc, action) {
          const snapshot = doc.saveSnapshot()
          staged = []
          try {
            const result = action()
            if (staged.length) batches.push(staged)
            return result
          } catch (error) {
            doc.restoreSnapshot(snapshot)
            throw error
          } finally {
            doc.discardSnapshot(snapshot)
          }
        },
        onBuffer: (pending) => {
          order.push(pending)
        },
        onDraftLost: lost,
        onCaretLost: caretLost,
        onMutation: (_doc, method, _args, call) => {
          if (method === 'restoreSnapshot') throw new Error('No collaborative inverse')
          staged.push(method)
          order.push(method)
          return call()
        },
      }
    }
  }
  const session = createStudioCollab(frame)!
  let doc: HwpDocument | null = null
  const wasm = { borrowDocumentHandle: () => doc }
  session.connect(wasm)
  const raw = opts.nested
    ? new HwpDocument(readFileSync(join(__dirname, 'fixtures', 'nested-form.hwp')))
    : HwpDocument.createEmpty()
  if (!opts.nested) {
    raw.createBlankDocument()
    raw.insertText(0, 0, 0, 'AB')
  }
  doc = session.wrap(wasm, raw)
  cleanup.push(() => raw.free())
  const field = document.querySelector('textarea')!
  const preview = document.querySelector('output')!
  const events = new EventTarget()
  let head: Pos = { sectionIndex: 0, paragraphIndex: 0, charOffset: 1 }
  let anchor: Pos | null = null
  const cursor = {
    recoveryCheckpoint: () => {
      const saved = { head, anchor }
      return {
        restore() {
          head = saved.head
          anchor = saved.anchor
        },
      }
    },
    getPosition: () => head,
    getSelection: () => anchor && { anchor, focus: head },
    setAnchor: () => {
      anchor = head
    },
    moveTo: (p: Pos) => {
      head = p
    },
    getRect: () => JSON.parse(raw.getCursorRect(0, 0, head.charOffset)),
    isInCellSelectionMode: () => false,
    isInTableObjectSelection: () => false,
    isInPictureObjectSelection: () => false,
    invalidateFocusedCellCursorGeometry: vi.fn(),
    resetPreferredX: vi.fn(),
  }
  const ih = {
    wasm,
    active: true,
    textarea: field,
    cursor,
    _isIOS: !!opts.ios,
    history: { clear: vi.fn() },
    cancelDeferredPaginationFlush: vi.fn(),
    deferredPaginationRunner: { cancel: vi.fn() },
    clearTableResizeRuntimeCache: vi.fn(),
    updateCaret: vi.fn(),
    updateViewportOverlayPositions: vi.fn(),
    resetDerivedStateAfterHistoryJump: () => {
      anchor = null
    },
    removeFormOverlay: vi.fn(),
    restoreEditContextAfterHistory: (_: unknown, p: Pos) => {
      head = p
    },
    afterEdit: vi.fn(() => events.dispatchEvent(new Event('cursor-rect-updated'))),
    updateSelection: vi.fn(),
    isInSeparateCaretSpace: () => false,
    caret: {
      showComposition: (rect: { x: number }, _width: number, _zoom: number, text: string) => {
        preview.textContent = text
        preview.style.left = `${rect.x}px`
      },
      hideComposition: () => {
        preview.textContent = ''
      },
    },
    viewportManager: { getZoom: () => 1 },
    eventBus: {
      emit(name: string, ...args: unknown[]) {
        if (!session.deferEvent(this, name, args)) events.dispatchEvent(new Event(name))
      },
      on: (name: string, cb: () => void) => {
        events.addEventListener(name, cb)
        return () => events.removeEventListener(name, cb)
      },
    },
  }
  const commands = new Map<string, (s: unknown) => void>()
  const host: HwpPresenceHost = {
    read: (fn) => fn(doc!),
    events: ih.eventBus,
    automation: {
      registerCommand: (cmd) => {
        commands.set(cmd.id, cmd.execute)
      },
      execute: (id) => commands.get(id)!({ getInputHandler: () => ih }),
      unregisterCommand: (id) => {
        commands.delete(id)
      },
    },
  }
  session.attachHost(host)
  const commit = vi.fn((text: string) => {
    doc!.insertText(0, 0, head.charOffset, text)
    head = { ...head, charOffset: head.charOffset + [...text].length }
    field.value = ''
  })
  const unbind = session.bindInput(ih, commit)!
  cleanup.push(unbind)
  field.addEventListener('input', () => session.transaction(wasm, () => commit(field.value)))
  const composition = (type: string, data: string) =>
    field.dispatchEvent(new parent.CompositionEvent(type, { data, bubbles: true }))
  const input = (text: string, inputType = 'insertText', isComposing = false) => {
    field.value = text
    field.dispatchEvent(
      new parent.InputEvent('input', { inputType, isComposing, data: text, bubbles: true }),
    )
  }
  const text = () => raw.getTextRange(0, 0, 0, raw.getParagraphLength(0, 0))
  return {
    frame,
    parent,
    field,
    cursor,
    session,
    raw,
    wasm,
    doc,
    controls,
    preview,
    composition,
    input,
    commit,
    ih,
    order,
    batches,
    text,
    lost,
    caretLost,
  }
}

const at = (offset: number) => ({ section: 0, paragraph: 0, path: [], offset })

describe('Studio collaboration input', () => {
  it('publishes a selection replacement once after the outer command returns', () => {
    const e = editor()
    e.controls.setEditable(true)
    const changed = vi.fn()
    e.ih.eventBus.on('document-mutated', changed)
    const result = e.session.transaction(e.wasm, () => {
      e.session.transaction(e.wasm, () => e.doc.deleteText(0, 0, 0, 1))
      e.ih.eventBus.emit('document-mutated')
      e.doc.insertText(0, 0, 0, '새')
      expect(e.batches).toEqual([])
      expect(changed).not.toHaveBeenCalled()
      return 17
    })
    expect(result).toBe(17)
    expect(e.text()).toBe('새B')
    expect(e.batches).toEqual([['deleteText', 'insertText']])
    expect(changed).toHaveBeenCalledOnce()
  })

  it.each(['caught native refusal', 'snapshot rollback', 'outer command error', 'dialog refusal'])(
    'restores the model and original backward selection without publishing after %s',
    (failure) => {
      const e = editor()
      e.controls.setEditable(true)
      e.cursor.moveTo({ sectionIndex: 0, paragraphIndex: 0, charOffset: 2 })
      e.cursor.setAnchor()
      e.cursor.moveTo({ sectionIndex: 0, paragraphIndex: 0, charOffset: 0 })
      const selection = e.cursor.getSelection()
      const changed = vi.fn(),
        refreshed = vi.fn()
      e.ih.eventBus.on('document-mutated', changed)
      e.ih.eventBus.on('document-view-changed', refreshed)
      const snapshot = e.doc.saveSnapshot()
      try {
        expect(() =>
          e.session.transaction(e.wasm, () => {
            e.doc.deleteText(0, 0, 0, 2)
            e.ih.resetDerivedStateAfterHistoryJump()
            e.ih.eventBus.emit('document-mutated')
            if (failure === 'outer command error') throw new Error('refused')
            if (failure === 'dialog refusal') return e.session.confirmDialog(() => false)
            if (failure === 'snapshot rollback') {
              expect(e.session.abortSnapshot(e.wasm, new Error('refused'))).toBe(true)
              return
            }
            try {
              e.doc.restoreSnapshot(snapshot)
            } catch {
              /* Studio tried its fallback. */
            }
          }),
        ).toThrow()
      } finally {
        e.doc.discardSnapshot(snapshot)
      }
      expect(e.batches).toEqual([])
      expect(e.text()).toBe('AB')
      expect(e.cursor.getSelection()).toEqual(selection)
      expect(changed).not.toHaveBeenCalled()
      expect(refreshed).toHaveBeenCalledOnce()
      expect(e.ih.history.clear).toHaveBeenCalledOnce()
    },
  )

  it('rolls back an edit rather than committing its synchronous prefix at an await', () => {
    const e = editor()
    e.controls.setEditable(true)
    expect(() =>
      e.session.transaction(e.wasm, () => {
        e.doc.deleteText(0, 0, 0, 2)
        return Promise.resolve()
      }),
    ).toThrow(/await/)
    expect(e.text()).toBe('AB')
    expect(e.batches).toEqual([])
  })

  it('does not admit an unscoped write after an asynchronous preparation yields', async () => {
    const e = editor()
    e.controls.setEditable(true)
    const later = e.session.transaction(e.wasm, async () => {
      await Promise.resolve()
      e.doc.deleteText(0, 0, 0, 2)
    })
    e.session.transaction(e.wasm, () => e.doc.insertText(0, 0, 2, '!'))
    await expect(later).rejects.toThrow(/transaction/)
    expect(e.text()).toBe('AB!')
    expect(e.batches).toEqual([['insertText']])
  })

  it('keeps Korean preedit outside WASM, reprojects after remote replay, and commits once at the mapped caret', () => {
    const e = editor()
    e.controls.setEditable(true)
    e.composition('compositionstart', '')
    e.composition('compositionupdate', 'ㅎ')
    e.input('하', 'insertCompositionText', true)
    expect(e.preview.textContent).toBe('하')
    expect(e.text()).toBe('AB')
    expect(e.session.locked).toBe(true)
    const beforeLeft = e.preview.style.left
    e.controls.replay(
      (raw) => raw.insertText(0, 0, 0, 'X'),
      (before) => {
        expect(before).toEqual({ anchor: at(1), head: at(1) })
        return { anchor: at(2), head: at(2) }
      },
    )
    expect(e.preview.textContent).toBe('하')
    expect(e.preview.style.left).not.toBe(beforeLeft)
    e.composition('compositionend', '한')
    e.input('한', 'insertFromComposition')
    expect(e.text()).toBe('XA한B')
    expect(e.commit.mock.calls).toEqual([['한']])
    expect(e.order).toEqual([true, 'insertText', false])
    expect(e.preview.textContent).toBe('')
    expect(e.session.locked).toBe(false)
  })

  it('returns a displaced draft for local recovery and restores a valid caret without committing it', () => {
    const e = editor()
    e.controls.setEditable(true)
    e.composition('compositionstart', '')
    e.input('하', 'insertCompositionText', true)
    e.controls.replay(
      (doc) => doc.deleteText(0, 0, 0, 2),
      () => ({ anchor: at(1), head: at(2) }),
    )
    expect(e.cursor.getPosition().charOffset).toBe(0)
    expect(e.preview.textContent).toBe('')
    e.composition('compositionend', '한')
    e.input('한', 'insertFromComposition')
    expect(e.text()).toBe('')
    expect(e.lost.mock.calls).toEqual([['하']])
    expect(e.order).toEqual([true, false])
  })

  it('preserves backward selection through replay and binds presence to the real frame host', () => {
    const e = editor()
    const presence = e.controls.bindPresence(vi.fn())
    e.cursor.moveTo({ sectionIndex: 0, paragraphIndex: 0, charOffset: 2 })
    e.cursor.setAnchor()
    e.cursor.moveTo({ sectionIndex: 0, paragraphIndex: 0, charOffset: 1 })
    e.controls.replay(
      (doc) => doc.insertText(0, 0, 0, 'x'),
      (sel) => {
        expect(sel).toEqual({ anchor: at(2), head: at(1) })
        return { anchor: at(3), head: at(2) }
      },
    )
    expect(presence.read()).toEqual({ anchor: at(3), head: at(2) })
    expect(e.ih.afterEdit).toHaveBeenCalledOnce()
    presence.dispose()
    expect(presence.read()).toBeNull()
  })

  it('keeps the caret behind a selected picture through a remote replay', () => {
    const e = editor()
    e.controls.setEditable(true)
    e.cursor.isInPictureObjectSelection = () => true
    e.controls.replay(
      (doc) => doc.insertText(0, 0, 0, 'x'),
      (sel) => {
        expect(sel).toEqual({ anchor: at(1), head: at(1) })
        return { anchor: at(2), head: at(2) }
      },
    )
    e.cursor.isInPictureObjectSelection = () => false
    e.input('!')
    expect(e.text()).toBe('xA!B')
    expect(e.caretLost).not.toHaveBeenCalled()
  })

  it('keeps a stranded caret where it was and unlocks only on a click on the page', () => {
    const e = editor()
    e.controls.setEditable(true)
    e.controls.replay(
      (doc) => doc.insertText(0, 0, 0, 'x'),
      () => null,
    )
    expect(e.caretLost).toHaveBeenCalledOnce()
    expect(e.cursor.getPosition().charOffset).toBe(1)
    expect(e.session.locked).toBe(true)
    expect(() => e.doc.insertText(0, 0, 0, '!')).toThrow(/read-only/)
    const send = (selector: string, event: Event) =>
      e.frame.document.querySelector(selector)!.dispatchEvent(event)
    // A menu, a key or the scrollbar does not place the caret: input stays locked.
    send('#menu-bar', new e.parent.Event('pointerdown', { bubbles: true }))
    send('#scroll-container', new e.parent.Event('pointerdown', { bubbles: true }))
    for (const key of ['ArrowRight', 'Home', 'b'])
      send('textarea', new e.parent.KeyboardEvent('keydown', { key, bubbles: true }))
    expect(e.session.locked).toBe(true)
    send('#scroll-content', new e.parent.Event('pointerdown', { bubbles: true }))
    expect(e.session.locked).toBe(false)
    e.input('!')
    expect(e.text()).toBe('x!AB')
  })

  it('rebuilds both ends in the inner table without confusing the owner paragraph with the cell paragraph', () => {
    const e = editor({ nested: true })
    const path = [
      { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
      { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
    ]
    const cell = {
      sectionIndex: 0,
      paragraphIndex: 0,
      parentParaIndex: 2,
      cellPath: path,
      ...path[0],
    }
    e.cursor.moveTo({ ...cell, charOffset: 4 })
    e.cursor.setAnchor()
    e.cursor.moveTo({ ...cell, charOffset: 1 })
    e.controls.replay(
      (doc) => doc.insertTextInCellByPath(0, 2, JSON.stringify(path), 0, '새'),
      (before) => ({
        anchor: { ...before.anchor, offset: before.anchor.offset + 1 },
        head: { ...before.head, offset: before.head.offset + 1 },
      }),
    )
    expect(e.raw.getTextInCellByPath(0, 2, JSON.stringify(path), 0, 1)).toBe('새')
    expect(e.cursor.getSelection()).toEqual({
      anchor: { ...cell, charOffset: 5 },
      focus: { ...cell, charOffset: 2 },
    })
  })

  it('blocks local writes when locked; a later unlock cannot commit a cancelled draft', () => {
    const e = editor()
    expect(() => e.doc.insertText(0, 0, 0, '!')).toThrow(/read-only/)
    e.controls.setEditable(true)
    e.composition('compositionstart', '')
    e.input('ㅎ', 'insertCompositionText', true)
    e.controls.setEditable(false)
    e.controls.withDocument((d) => d.insertText(0, 0, 0, 'X'))
    e.controls.setEditable(true)
    e.composition('compositionend', '한')
    e.input('한', 'insertFromComposition')
    e.input('!')
    expect(e.text()).toBe('X!AB')
    expect(e.lost.mock.calls).toEqual([['ㅎ']])
    expect(e.order).toEqual([true, false, 'insertText'])
    e.frame.dispatchEvent(new e.parent.Event('pagehide'))
    expect(() => e.controls.withDocument((d) => d.pageCount())).toThrow()
    expect(() => e.doc.insertText(0, 0, 0, '!')).toThrow(/read-only/)
  })

  it.each([{ mode: 'view' }, { readonly: true }])(
    'never promotes an immutable viewer (%o)',
    (opts) => {
      const e = editor(opts)
      e.controls.setEditable(true)
      expect(e.session.locked).toBe(true)
      e.composition('compositionstart', '')
      e.composition('compositionend', '한')
      expect(() => e.doc.insertText(0, 0, 0, 'local')).toThrow(/read-only/)
      e.controls.replay(
        (d) => d.insertText(0, 0, 0, 'peer'),
        (selection) => selection,
      )
      expect(e.text()).toBe('peerAB')
      expect(e.order).toEqual([])
    },
  )

  it('buffers eventless iOS composition until blur and accepts an explicit native final boundary once', () => {
    const e = editor({ ios: true })
    e.controls.setEditable(true)
    e.input('ㄱ')
    e.input('가')
    expect(e.text()).toBe('AB')
    e.field.dispatchEvent(new e.parent.FocusEvent('blur'))
    e.input('ㄴ')
    e.input('나', 'insertFromComposition')
    e.composition('compositionend', '나')
    expect(e.text()).toBe('A가나B')
    expect(e.order).toEqual([true, 'insertText', false, true, 'insertText', false])
  })

  it('fails closed without registration; an unmarked frame has no collaboration hooks', () => {
    const e = editor({ registered: false })
    expect(e.session.locked).toBe(true)
    expect(() => e.doc.insertText(0, 0, 0, 'local')).toThrow(/read-only/)
    e.parent.document.documentElement.removeAttribute('data-studio-collab')
    expect(createStudioCollab(e.frame)).toBeNull()
    expect(createStudioCollab(e.parent as unknown as Window)).toBeNull()
  })
})
