import { wrapHwpMutations } from './collab-capture.mjs'
import { bindHwpPresence, readHwpSelection, validateHwpSelection } from './collab-presence.mjs'
import { askedReadOnly, readOnlyDocument } from './read-only.mjs'

/** The parent registers before creating the frame and accepts only its own editor's contentWindow. */
export function createStudioCollab(win) {
  let parent
  try {
    parent = win?.parent
    if (
      !parent ||
      parent === win ||
      parent.location.origin !== win.location.origin ||
      !parent.document.documentElement.hasAttribute('data-studio-collab')
    )
      return null
  } catch {
    return null
  }
  const viewer =
    askedReadOnly(win) || new URLSearchParams(parent.location.search).get('mode') !== 'edit'
  const raw = new WeakMap()
  const owner = parent.seuHwpCollabConnect
  let primary, callbacks, input, handler, host, presence
  let editable = false
  let closed = false
  let replaying = false
  const locked = () => closed || replaying || viewer || !editable || !callbacks
  const controls = Object.freeze({
    setEditable(next) {
      editable = next === true && !viewer && !closed
      if (!editable) input?.cancel()
    },
    withDocument(fn) {
      const doc = !closed && raw.get(primary?.borrowDocumentHandle())
      if (!doc) throw new Error('No current collaboration document')
      return fn(doc)
    },
    bindPresence(publish) {
      if (closed || !host) throw new Error('No active Studio plugin host')
      presence?.dispose()
      presence = bindHwpPresence(host, publish)
      return presence
    },
    replay(apply, map) {
      if (!handler || !host || replaying) throw new Error('No active Studio input')
      return controls.withDocument((doc) => {
        let before
        try {
          before = readHwpSelection(handler, doc)
        } catch {
          before = null
        }
        replaying = true
        let mapped
        try {
          const result = apply(doc, before)
          mapped = before && validateHwpSelection(doc, map(before))
          return result
        } finally {
          try {
            if (!mapped) input.cancel()
            restoreSelection(handler, mapped)
          } finally {
            replaying = false
          }
        }
      })
    },
  })
  const shutdown = () => {
    closed = true
    input?.dispose()
    presence?.dispose()
    handler = host = null
    callbacks = null
    win.removeEventListener('pagehide', shutdown)
  }
  win.addEventListener('pagehide', shutdown)
  return {
    get locked() {
      return Boolean(locked() || input?.buffering())
    },
    attachHost(next) {
      if (!closed) host = next
    },
    connect(bridge) {
      if (primary || closed) throw new Error('Studio collaboration already bound')
      primary = bridge
      if (typeof owner !== 'function') return
      const hooks = owner(win, controls)
      if (typeof hooks?.onMutation === 'function' && typeof hooks.onBuffer === 'function')
        callbacks = hooks
    },
    wrap(bridge, doc) {
      if (bridge !== primary) return viewer ? readOnlyDocument(doc) : doc
      if (raw.has(bridge.borrowDocumentHandle())) editable = false
      input?.cancel()
      const wrapped = wrapHwpMutations(doc, (target, method, args, call) => {
        if (locked() || input?.buffering() || bridge.borrowDocumentHandle() !== wrapped) {
          throw new Error(`read-only document: ${method} would change it`)
        }
        return callbacks.onMutation(target, method, args, call)
      })
      raw.set(wrapped, doc)
      return wrapped
    },
    bindInput(ih, commit) {
      if (ih.wasm !== primary || closed) return
      input?.dispose()
      const ime = bindComposition(
        ih,
        commit,
        locked,
        (pending) => callbacks?.onBuffer(pending),
        (text) => callbacks?.onDraftLost?.(text),
      )
      input = ime
      handler = ih
      return () => {
        ime.dispose()
        if (input === ime) {
          input = handler = null
        }
      }
    },
  }
}

function toCursor({ section, paragraph, path, offset }) {
  const pos = { sectionIndex: section, paragraphIndex: paragraph, charOffset: offset }
  if (path.length)
    Object.assign(pos, path[0], {
      parentParaIndex: paragraph,
      paragraphIndex: path.at(-1).cellParaIndex,
      cellPath: path,
    })
  return pos
}

function restoreSelection(ih, mapped) {
  // Studio's command stack and object handles refer to the pre-replay document.
  ih.history.clear(ih.wasm)
  ih.resetDerivedStateAfterHistoryJump()
  ih.removeFormOverlay()
  ih.cursor.invalidateFocusedCellCursorGeometry()
  const anchor = mapped
    ? toCursor(mapped.anchor)
    : { sectionIndex: 0, paragraphIndex: 0, charOffset: 0 }
  ih.restoreEditContextAfterHistory(null, anchor)
  if (
    mapped &&
    (mapped.anchor.section !== mapped.head.section ||
      mapped.anchor.paragraph !== mapped.head.paragraph ||
      mapped.anchor.offset !== mapped.head.offset ||
      mapped.anchor.path.at(-1)?.cellParaIndex !== mapped.head.path.at(-1)?.cellParaIndex)
  ) {
    ih.cursor.setAnchor()
    ih.cursor.moveTo(toCursor(mapped.head))
  }
  ih.cursor.resetPreferredX()
  ih.afterEdit()
  ih.updateSelection()
}

/** A draft lives in the native input, never in the document. Only its final input uses Studio's edit path. */
function bindComposition(ih, commit, locked, onBuffer, onDraftLost) {
  const field = ih.textarea
  const page = field.ownerDocument
  const win = page.defaultView
  const off = []
  let phase = 'idle' // explicit composition, implicit iOS input, or cancelled until native end
  let preview = ''
  let echo = null
  let nativeEnd = null
  let echoTimer
  const buffering = () => phase === 'explicit' || phase === 'implicit'
  const forgetEcho = () => {
    win.clearTimeout(echoTimer)
    echo = null
  }
  const paint = () => {
    if (!buffering() || !preview) return
    const at = ih.cursor.getRect()
    if (!at) return ih.caret.hideComposition()
    ih.caret.showComposition(
      at,
      at.height * preview.length,
      ih.viewportManager.getZoom(),
      preview,
      'sans-serif',
    )
  }
  function begin(next) {
    const wasBuffered = buffering()
    phase = !ih.active || locked() ? 'cancelled' : next
    if (buffering() !== wasBuffered) onBuffer(buffering())
  }
  function finish(text, cancelled = false) {
    const wasBuffered = buffering()
    phase = cancelled ? 'cancelled' : 'idle'
    preview = ''
    field.value = ''
    ih.caret.hideComposition()
    try {
      if (wasBuffered && !cancelled && !locked() && ih.active && text) {
        // The owner may have mapped the live caret/selection while the IME was open.
        commit(text)
      }
    } finally {
      if (wasBuffered) onBuffer(false)
    }
  }
  function markEcho(text) {
    forgetEcho()
    nativeEnd = text
    echo = text
    echoTimer = win.setTimeout(forgetEcho, 0)
  }
  const cancel = () => {
    if (!buffering()) return
    const draft = preview || field.value
    try {
      finish('', true)
    } finally {
      if (draft) onDraftLost(draft)
    }
  }
  function listen(node, type, handler) {
    node.addEventListener(type, handler, true)
    off.push(() => node.removeEventListener(type, handler, true))
  }
  listen(field, 'compositionstart', (e) => {
    e.stopImmediatePropagation()
    nativeEnd = null
    forgetEcho()
    if (phase === 'implicit') finish(field.value)
    begin(ih._cellBlockLetterImeGuard?.consume('compositionstart') ? 'cancelled' : 'explicit')
  })
  listen(field, 'compositionupdate', (e) => {
    e.stopImmediatePropagation()
    if (phase === 'explicit') {
      preview = e.data
      if (preview) paint()
      else ih.caret.hideComposition()
    }
  })
  listen(field, 'input', (e) => {
    // Some keyboards send their final input after compositionend. A new physical key is not an echo.
    if (
      phase === 'idle' &&
      ((e.inputType === 'insertFromComposition' &&
        nativeEnd !== null &&
        (e.data ?? field.value) === nativeEnd) ||
        (echo !== null && (e.data ?? field.value) === echo))
    ) {
      e.stopImmediatePropagation()
      nativeEnd = null
      forgetEcho()
      field.value = ''
      return
    }
    nativeEnd = null
    if (e.inputType === 'insertFromComposition') {
      e.stopImmediatePropagation()
      if (phase === 'idle') begin('explicit')
      const text = e.data ?? field.value
      finish(text)
      markEcho(text)
      return
    }
    if (phase === 'idle' && (e.isComposing || e.inputType === 'insertCompositionText'))
      begin('explicit')
    if (phase === 'idle' && ih._isIOS) begin('implicit')
    if (phase === 'idle' && !locked()) return
    e.stopImmediatePropagation()
    if (buffering()) {
      preview = field.value
      if (preview) paint()
      else ih.caret.hideComposition()
    } else field.value = ''
  })
  listen(field, 'compositionend', (e) => {
    e.stopImmediatePropagation()
    const final = typeof e.data === 'string' ? e.data : ''
    // Even a cancelled session can receive its browser's duplicate final input after unlocking.
    markEcho(final)
    finish(final)
  })
  listen(field, 'keydown', (e) => {
    nativeEnd = null
    forgetEcho()
    if (phase === 'explicit' || (phase === 'cancelled' && (e.isComposing || e.keyCode === 229))) {
      e.stopImmediatePropagation() // let the IME, not the document shortcuts, consume this key
    } else if (
      phase === 'implicit' &&
      !e.isComposing &&
      (e.ctrlKey ||
        e.metaKey ||
        ['Enter', 'Tab', 'Escape', 'Home', 'End', 'PageDown', 'PageUp'].includes(e.key) ||
        e.key.startsWith('Arrow'))
    ) {
      finish(e.key === 'Escape' ? '' : field.value)
    } else if (phase === 'cancelled') phase = 'idle'
  })
  // WebKit's contenteditable input can compose with no composition events. Blur or a navigation
  // boundary commits its actual DOM value; a quiet timer cannot tell a pause from a commit.
  const finishImplicit = () => {
    if (phase === 'implicit') finish(field.value)
    else if (phase === 'cancelled' && ih._isIOS) phase = 'idle'
  }
  listen(field, 'blur', finishImplicit)
  listen(page, 'pointerdown', (e) => {
    if (e.target !== field) finishImplicit()
  })
  for (const name of ['cursor-rect-updated', 'zoom-changed', 'document-layout-refreshed']) {
    off.push(ih.eventBus.on(name, paint))
  }
  return {
    buffering,
    cancel,
    dispose() {
      cancel()
      forgetEcho()
      for (const unsubscribe of off.splice(0)) unsubscribe()
    },
  }
}

export const studioCollab = createStudioCollab(globalThis.window)
