import type { Json } from '@seuoffice/collab/client'
import { presenceLabel, type PresenceMark } from '@seuoffice/web-platform/presence'
import type { Editor } from '@tiptap/core'
import { Plugin, PluginKey } from '@tiptap/pm/state'
import { Decoration, DecorationSet } from '@tiptap/pm/view'
import { getActiveSubEditor, subscribeSubEditorState } from '../renderer/editor/active-editor'

type BodySelection = { anchor: number; head: number }

function bodySelection(at: Json, end: number): at is BodySelection {
  if (!at || typeof at !== 'object' || Array.isArray(at)) return false
  const valid = (pos: Json | undefined) =>
    typeof pos === 'number' && Number.isInteger(pos) && pos >= 0 && pos <= end
  return valid(at.anchor) && valid(at.head)
}

function caret(peer: PresenceMark): HTMLElement {
  const span = document.createElement('span')
  span.className = 'docs-presence-caret'
  span.hidden = true
  span.style.borderInlineStartColor = peer.color
  const label = presenceLabel(peer)
  label.style.insetInlineStart = '0'
  label.style.bottom = '100%'
  span.append(label)
  return span
}

export function docsPresence({ editor }: { editor: Editor }, publish: () => void) {
  const key = new PluginKey<DecorationSet>('docsPeerSelection')
  const doc = editor.view.dom.ownerDocument
  // Measure the layer itself: the page can be zoomed, clipped or scrolled by an ancestor.
  const layer = doc.createElement('div')
  layer.className = 'docs-presence-layer'
  editor.view.dom.after(layer)
  let carets: { head: number; el: HTMLElement }[] = []
  let frame = 0
  const place = () => {
    frame = 0
    const rect = layer.getBoundingClientRect()
    if (!rect.width) return
    const scale = rect.width / layer.offsetWidth
    for (const { head, el } of carets) {
      el.hidden = !editor.state.doc.resolve(head).parent.isTextblock
      if (el.hidden) continue
      const { left, top, bottom } = editor.view.coordsAtPos(head)
      el.style.left = `${(left - rect.left) / scale}px`
      el.style.top = `${(top - rect.top) / scale}px`
      el.style.height = `${(bottom - top) / scale}px`
    }
  }
  const schedule = () => {
    if (carets.length && !frame) frame = requestAnimationFrame(place)
  }
  const resize = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(schedule)
  resize?.observe(editor.view.dom)
  doc.addEventListener('scroll', schedule, true)
  doc.fonts?.addEventListener('loadingdone', schedule)
  editor.on('transaction', ({ transaction, appendedTransactions }) => {
    for (const caret of carets) {
      caret.head = transaction.mapping.map(caret.head, -1)
      for (const tr of appendedTransactions) caret.head = tr.mapping.map(caret.head, -1)
    }
    schedule()
  })
  const viewerSelectionChanged = () => {
    if (!editor.isEditable) publish()
  }
  editor.registerPlugin(
    new Plugin<DecorationSet>({
      key,
      state: {
        init: () => DecorationSet.empty,
        apply: (tr, previous) => tr.getMeta(key) ?? previous.map(tr.mapping, tr.doc),
      },
      props: { decorations: (state) => key.getState(state) },
    }),
  )
  editor.on('selectionUpdate', publish)
  const unsubscribe = subscribeSubEditorState(publish)
  doc.addEventListener('selectionchange', viewerSelectionChanged)
  editor.on('destroy', () => {
    unsubscribe()
    doc.removeEventListener('selectionchange', viewerSelectionChanged)
    doc.removeEventListener('scroll', schedule, true)
    doc.fonts?.removeEventListener('loadingdone', schedule)
    resize?.disconnect()
    cancelAnimationFrame(frame)
    layer.remove()
  })

  let drawId = 0
  return {
    read(): Json {
      if (editor.isDestroyed || getActiveSubEditor()) return null
      if (editor.isEditable) {
        const { anchor, head } = editor.state.selection
        return { anchor, head }
      }
      // Read-only ProseMirror does not always own focus or mirror the browser selection.
      const selection = doc.getSelection()
      const { view } = editor
      if (
        !selection?.anchorNode ||
        !selection.focusNode ||
        !view.dom.contains(selection.anchorNode) ||
        !view.dom.contains(selection.focusNode)
      )
        return null
      return {
        anchor: view.posAtDOM(selection.anchorNode, selection.anchorOffset),
        head: view.posAtDOM(selection.focusNode, selection.focusOffset),
      }
    },
    draw(peers: readonly PresenceMark[]): void {
      if (editor.isDestroyed) return
      const id = ++drawId
      const body = editor.state.doc
      // A status callback can run inside another transaction's view update.
      queueMicrotask(() => {
        if (id !== drawId || editor.isDestroyed) return
        const marks: Decoration[] = []
        carets = []
        for (const peer of body === editor.state.doc ? peers : []) {
          if (!bodySelection(peer.at, body.content.size)) continue
          const { anchor, head } = peer.at
          if (anchor !== head) {
            marks.push(
              Decoration.inline(Math.min(anchor, head), Math.max(anchor, head), {
                class: 'docs-presence-selection',
                style: `background-color:${peer.color}44`,
              }),
            )
          }
          if (body.resolve(head).parent.isTextblock) carets.push({ head, el: caret(peer) })
        }
        const next = DecorationSet.create(body, marks)
        if (next !== key.getState(editor.state)) {
          editor.view.dispatch(editor.state.tr.setMeta(key, next))
        }
        layer.replaceChildren(...carets.map(({ el }) => el))
        schedule()
      })
    },
  }
}
