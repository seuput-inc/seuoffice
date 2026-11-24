import type { CollabStatus, Json } from '@seuoffice/collab/client'

export interface PresenceMark {
  client: string
  name: string
  color: string
  at: Json
}

const colors = ['#1261a0', '#8d368f', '#19734e', '#a53d30', '#6a47a3', '#866017']

function presenceColor(client: string): string {
  let hash = 0
  for (let i = 0; i < client.length; i++) hash = (Math.imul(hash, 31) + client.charCodeAt(i)) >>> 0
  return colors[hash % colors.length]!
}

export function presenceMarks(status: CollabStatus): PresenceMark[] {
  const marks: PresenceMark[] = []
  if (status.phase !== 'live' || status.unconfirmed !== 0) return marks
  for (const { client, name } of status.peers) {
    if (client === status.self?.client || !Object.hasOwn(status.presences, client)) continue
    const location = status.presences[client]!
    if (location.v === status.version && location.at !== null)
      marks.push({ client, name, color: presenceColor(client), at: location.at })
  }
  return marks
}

export function presenceLabel(
  peer: Pick<PresenceMark, 'client' | 'name' | 'color'>,
): HTMLSpanElement {
  const label = document.createElement('span')
  label.dataset.presenceClient = peer.client
  label.textContent = peer.name
  label.dir = 'auto'
  label.contentEditable = 'false'
  Object.assign(label.style, {
    position: 'absolute',
    pointerEvents: 'none',
    userSelect: 'none',
    backgroundColor: peer.color,
    color: 'white',
    borderRadius: '3px',
    padding: '1px 4px',
    font: '600 11px/15px system-ui, sans-serif',
    whiteSpace: 'nowrap',
    maxWidth: '16em',
    overflow: 'hidden',
    textOverflow: 'ellipsis',
    zIndex: '1',
  })
  return label
}
