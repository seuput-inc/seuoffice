import { wrapHwpMutations } from './collab-capture.mjs'
import {
  bindHwpPresence,
  readHwpCaret,
  readHwpSelection,
  validateHwpSelection,
} from './collab-presence.mjs'
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
  // A replay that cannot place the caret keeps input locked until the user clicks a new place.
  let stranded = false
  let command
  function run(action) {
    try {
      if (command.failure) throw command.failure.error
      const result = action()
      if (command.failure) throw command.failure.error
      if (command.writes && typeof result?.then === 'function')
        throw new Error('A Studio edit cannot commit across an await')
      return result
    } catch (error) {
      // Studio catches some native failures and tries a fallback. Neither path can commit.
      command.failure ??= { error }
      throw command.failure.error
    }
  }
  function transaction(bridge, action) {
    if (bridge !== primary || locked() || input?.buffering()) return action()
    if (command) return run(action)
    const doc = raw.get(bridge.borrowDocumentHandle())
    if (!doc) return action()
    const cursor = handler?.cursor.recoveryCheckpoint()
    const editing = handler && {
      pendingCharShape: handler.pendingCharShape,
      pendingCharShapeAnchor: handler.pendingCharShapeAnchor,
      fieldStartExitKey: handler.fieldStartExitKey,
      fieldEndExitKey: handler.fieldEndExitKey,
      pastedFieldEndOutsidePending: handler.pastedFieldEndOutsidePending,
    }
    const events = []
    const state = { events, writes: false }
    let result
    try {
      result = callbacks.transaction(doc, () => {
        command = state
        try {
          const value = run(action)
          if (
            state.writes &&
            (locked() || input?.buffering() || raw.get(bridge.borrowDocumentHandle()) !== doc)
          )
            throw new Error('The Studio edit lost its document before commit')
          return value
        } finally {
          command = undefined
        }
      })
    } catch (error) {
      // The owner has already restored the model. No command or undo handle may refer to its old state.
      if (cursor && handler && raw.get(bridge.borrowDocumentHandle()) === doc) {
        handler.history.clear(bridge)
        handler.cancelDeferredPaginationFlush()
        handler.deferredPaginationRunner.cancel()
        handler.deferredPaginationPending = false
        handler.pendingFocusedPagePatch = handler.lastCellKey = handler.protectedCellHitCache = null
        handler.clearTableResizeRuntimeCache()
        handler.resetDerivedStateAfterHistoryJump()
        handler.removeFormOverlay()
        Object.assign(handler, editing)
        cursor.restore()
        handler.updateCaret(true)
        handler.eventBus.emit('document-view-changed')
        handler.updateViewportOverlayPositions()
      }
      throw error
    }
    for (const [bus, event, args] of events) {
      try {
        bus.emit(event, ...args)
      } catch (error) {
        console.error('Studio observer failed after command commit:', error)
      }
    }
    return result
  }
  const locked = () => closed || replaying || stranded || viewer || !editable || !callbacks
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
          before = readHwpSelection(handler, doc) ?? readHwpCaret(handler, doc)
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
            if (!mapped) {
              input.cancel()
              if (editable && !stranded) {
                stranded = true
                callbacks?.onCaretLost?.()
              }
            }
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
  win.document.addEventListener(
    'pointerdown',
    () => {
      stranded = false
    },
    true,
  )
  return {
    get locked() {
      return Boolean(locked() || input?.buffering())
    },
    transaction,
    confirmDialog(apply) {
      return transaction(primary, () => {
        const result = apply()
        if (result === false && command?.writes)
          throw new Error('Studio dialog declined a partial edit')
        return result
      })
    },
    abortSnapshot(bridge, error) {
      if (!command || bridge !== primary) return false
      command.failure ??= { error }
      return true
    },
    deferEvent(bus, event, args) {
      if (!command || bus !== handler?.eventBus || !commitEvents.has(event)) return false
      command.events.push([bus, event, args])
      return true
    },
    get hasWrites() {
      return Boolean(command?.writes)
    },
    attachHost(next) {
      if (!closed) host = next
    },
    connect(bridge) {
      if (primary || closed) throw new Error('Studio collaboration already bound')
      primary = bridge
      if (typeof owner !== 'function') return
      const hooks = owner(win, controls)
      if (
        typeof hooks?.transaction === 'function' &&
        typeof hooks.onMutation === 'function' &&
        typeof hooks.onBuffer === 'function'
      )
        callbacks = hooks
    },
    wrap(bridge, doc) {
      if (bridge !== primary) return viewer ? readOnlyDocument(doc) : doc
      if (raw.has(bridge.borrowDocumentHandle())) editable = false
      input?.cancel()
      const wrapped = wrapHwpMutations(doc, (target, method, args, call) => {
        const apply = () => {
          if (locked() || input?.buffering() || bridge.borrowDocumentHandle() !== wrapped)
            throw new Error(`read-only document: ${method} would change it`)
          if (!command) throw new Error(`No Studio edit transaction for ${method}`)
          command.writes = true
          return callbacks.onMutation(target, method, args, call)
        }
        return command ? run(apply) : apply()
      })
      raw.set(wrapped, doc)
      return wrapped
    },
    bindInput(ih, commit) {
      if (ih.wasm !== primary || closed) return
      input?.dispose()
      const ime = bindComposition(
        ih,
        (text) => transaction(ih.wasm, () => commit(text)),
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
    } catch (error) {
      onDraftLost(text)
      throw error
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

const commitEvents = new Set([
  'document-mutated',
  'document-changed',
  'document-page-invalidated',
  'command-state-changed',
  'cursor-rect-updated',
  'cell-selection-phase-changed',
  'table-object-selection-changed',
  'picture-object-selection-changed',
  'headerFooterModeChanged',
  'footnoteModeChanged',
])

/** The frame is optional. Only the action's synchronous body is a Studio edit. */
export function runStudioTransaction(bridge, action) {
  return studioCollab ? studioCollab.transaction(bridge, action) : action()
}

export const studioCollab = createStudioCollab(globalThis.window)
