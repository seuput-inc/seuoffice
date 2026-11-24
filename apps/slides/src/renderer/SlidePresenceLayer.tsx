import React, { useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react'
import { createPortal } from 'react-dom'
import type { RenderNode, RenderSlide, ShapeRenderNode } from '@seuoffice/pptx-render'
import { presenceLabel } from '@seuoffice/web-platform/presence'
import {
  bodyKey,
  selectedRects,
  textSelection,
  type SlidePeer,
  type SlidePresence,
  type TextSelection,
} from './slide-presence'
import { slideTextLayout } from './TextEditOverlay'

function boxStyle(node: RenderNode): React.CSSProperties {
  const b = node.box
  return {
    position: 'absolute',
    left: b.x,
    top: b.y,
    width: b.w,
    height: b.h,
    transform: `rotate(${b.rotationDeg ?? 0}deg)`,
  }
}
const hasText = (n: RenderNode): n is ShapeRenderNode => n.type === 'shape' || n.type === 'text'

function Label({ peer: { client, name, color } }: { peer: SlidePeer }) {
  const host = useRef<HTMLSpanElement>(null)
  useLayoutEffect(() => {
    const label = presenceLabel({ client, name, color })
    Object.assign(label.style, {
      left: '0',
      bottom: '100%',
      transformOrigin: 'bottom left',
      transform: 'var(--slide-label-transform)',
    })
    host.current!.replaceChildren(label)
  }, [name, client, color])
  return <span ref={host} style={{ position: 'absolute', top: 0, left: 0 }} />
}

/** Lay the browser range out without rotation or zoom. Its rects then belong to the object's own axes. */
function TextMark({
  node,
  scale,
  peer,
  text,
}: {
  node: ShapeRenderNode
  scale: number
  peer: SlidePeer
  text: TextSelection
}) {
  const layout = useMemo(() => slideTextLayout(node, scale), [node, scale])
  const root = useRef<HTMLDivElement>(null)
  const [rects, setRects] = useState<DOMRect[]>([])
  useLayoutEffect(() => {
    layout.populate(root.current!, true)
  }, [layout])
  useLayoutEffect(() => {
    const measure = () => setRects(selectedRects(root.current!, text))
    measure()
    document.fonts.addEventListener('loadingdone', measure)
    return () => document.fonts.removeEventListener('loadingdone', measure)
  }, [layout, text])
  const caret = text.start[0] === text.end[0] && text.start[1] === text.end[1]
  return (
    <>
      {createPortal(
        <div
          aria-hidden="true"
          style={{
            ...layout.frame,
            position: 'fixed',
            top: 0,
            left: 0,
            transform: 'none',
            visibility: 'hidden',
            pointerEvents: 'none',
          }}
        >
          <div ref={root} className="slide-presence-text" style={layout.body} />
        </div>,
        document.body,
      )}
      {!rects.length && <Label peer={peer} />}
      {rects.map((r, i) => (
        <span
          key={i}
          data-slide-presence={caret ? 'caret' : 'range'}
          style={{
            position: 'absolute',
            left: r.x,
            top: r.y,
            width: Math.max(r.width, 2),
            height: Math.max(r.height, 2),
          }}
        >
          <span
            style={{
              position: 'absolute',
              inset: 0,
              background: peer.color,
              opacity: caret ? 1 : 0.3,
            }}
          />
          {i === 0 && <Label peer={peer} />}
        </span>
      ))}
    </>
  )
}

function ReaderText({ node, scale }: { node: ShapeRenderNode; scale: number }) {
  const layout = useMemo(() => slideTextLayout(node, scale), [node, scale])
  const ref = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    layout.populate(ref.current!, true)
  }, [layout])
  return (
    <div style={{ ...layout.frame, transform: 'none', left: 0, top: 0 }}>
      <div ref={ref} className="slide-presence-text slide-reader-text" style={layout.body} />
    </div>
  )
}

export function SlidePresenceLayer({
  presence,
  slide,
  selectedIds,
  editing,
  groupId,
  readerScale,
}: {
  presence: SlidePresence
  slide: RenderSlide
  selectedIds: string[]
  editing?: string
  groupId?: string | null
  readerScale?: number
}) {
  const peers = useSyncExternalStore(presence.subscribe, presence.snapshot)
  const reader = readerScale !== undefined
  const [viewIds, setViewIds] = useState<string[]>([])
  const [draft, setDraft] = useState<number | null>(null)
  const layer = useRef<HTMLDivElement>(null)
  const group =
    !reader && groupId
      ? slide.nodes.find((n) => n.sourceId === groupId && n.type === 'group')
      : undefined
  const nodes = group?.type === 'group' ? group.children : slide.nodes
  const ownIds = reader ? viewIds : selectedIds
  const editingId = reader ? (viewIds.length === 1 ? viewIds[0] : undefined) : editing
  useLayoutEffect(() => {
    presence.shownSlide = slide.partPath
    const editor = () =>
      reader
        ? (layer.current?.querySelector<HTMLElement>('.slide-reader-text') ?? null)
        : (layer.current?.parentElement?.querySelector<HTMLElement>('.slide-text-editor') ?? null)
    presence.read = () => {
      if (!slide.partPath) return null
      const selected = editingId ? [editingId] : ownIds
      const ids = nodes
        .filter((n) => !n.decoration && selected.includes(n.sourceId) && n.durableId)
        .map((n) => n.durableId!)
      if (!ids.length || (group && !group.durableId)) return null
      return {
        slide: slide.partPath,
        ids,
        group: group?.durableId ?? null,
        text: editingId ? textSelection(editor()) : null,
      }
    }
    const changed = () => {
      const root = !reader && editingId && editor()
      setDraft(root ? bodyKey(root) : null)
      presence.publish()
    }
    changed()
    document.addEventListener('selectionchange', changed)
    document.addEventListener('input', changed)
    return () => {
      document.removeEventListener('selectionchange', changed)
      document.removeEventListener('input', changed)
      presence.shownSlide = undefined
      presence.read = () => null
      presence.publish()
    }
  }, [presence, reader, slide, nodes, group, ownIds, editingId])

  return (
    <div
      ref={layer}
      style={{
        position: 'absolute',
        left: 0,
        top: 0,
        width: slide.widthPx,
        height: slide.heightPx,
        transformOrigin: 'top left',
        transform: reader ? `scale(${readerScale})` : undefined,
        pointerEvents: reader && viewIds.length ? 'auto' : 'none',
        zIndex: 21,
      }}
      onClick={(e) => {
        if (reader && e.target === e.currentTarget) {
          setViewIds([])
          e.stopPropagation()
        }
      }}
    >
      {reader &&
        slide.nodes.map(
          (n) =>
            !n.decoration &&
            n.durableId && (
              <div
                key={n.sourceId}
                data-slide-object={n.durableId}
                style={{
                  ...boxStyle(n),
                  pointerEvents: 'auto',
                  outline: viewIds.includes(n.sourceId) ? '2px solid Highlight' : undefined,
                }}
                onMouseDown={(e) => {
                  if (e.button !== 0) return
                  const toggle =
                    e.shiftKey &&
                    !(e.target instanceof Element && e.target.closest('.slide-reader-text'))
                  setViewIds((prev) =>
                    toggle
                      ? prev.includes(n.sourceId)
                        ? prev.filter((id) => id !== n.sourceId)
                        : [...prev, n.sourceId]
                      : prev.length === 1 && prev[0] === n.sourceId
                        ? prev
                        : [n.sourceId],
                  )
                }}
                onClick={(e) => {
                  e.stopPropagation()
                  e.preventDefault()
                }}
              >
                {viewIds.length === 1 && viewIds[0] === n.sourceId && hasText(n) && n.text && (
                  <ReaderText node={n} scale={slide.scale} />
                )}
              </div>
            ),
        )}
      {peers.map((peer) => {
        if (peer.at.slide !== slide.partPath) return null
        const g = peer.at.group
          ? slide.nodes.find(
              (n) => !n.decoration && n.durableId === peer.at.group && n.type === 'group',
            )
          : undefined
        if (peer.at.group && g?.type !== 'group') return null
        const objects = (g?.type === 'group' ? g.children : slide.nodes).filter(
          (n) => !n.decoration && n.durableId && peer.at.ids.includes(n.durableId),
        )
        const marks = objects.map((n, i) => {
          const text =
            hasText(n) &&
            peer.at.text &&
            (n.sourceId !== editing || draft === null || draft === peer.at.text.body)
              ? peer.at.text
              : null
          const style: React.CSSProperties & { '--slide-label-transform': string } = {
            ...boxStyle(n),
            pointerEvents: 'none',
            outline: `2px solid ${peer.color}`,
            '--slide-label-transform': `rotate(${-(n.box.rotationDeg ?? 0)}deg) scale(${g?.box.flipH ? -1 : 1}, ${g?.box.flipV ? -1 : 1}) rotate(${-(g?.box.rotationDeg ?? 0)}deg)`,
          }
          return (
            <div key={n.sourceId} data-slide-presence-object={n.durableId} style={style}>
              {text && hasText(n) ? (
                <TextMark node={n} scale={slide.scale} peer={peer} text={text} />
              ) : (
                i === 0 && <Label peer={peer} />
              )}
            </div>
          )
        })
        return g ? (
          <div
            key={peer.client}
            style={{
              ...boxStyle(g),
              pointerEvents: 'none',
              transform: `${boxStyle(g).transform} scale(${g.box.flipH ? -1 : 1}, ${g.box.flipV ? -1 : 1})`,
            }}
          >
            {marks}
          </div>
        ) : (
          <React.Fragment key={peer.client}>{marks}</React.Fragment>
        )
      })}
    </div>
  )
}
