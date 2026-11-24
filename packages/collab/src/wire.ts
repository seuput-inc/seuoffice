/**
 * The frames between the engine and the server (design section 4): JSON text
 * frames with protocol `p: 1`, and binary frames that carry nothing but a
 * base file. Both parsers are the boundary: they accept exactly these shapes,
 * rebuild them without unknown keys, and throw on anything else. Not exported
 * from the package; hosts relay frames without reading them.
 */

import { MAX_PRESENCE_BYTES } from './limits.js'
import type { Change, Format, Json, Peer } from './model.js'

export type ErrorCode =
  'bad_frame' | 'forbidden' | 'too_large' | 'schema_mismatch' | 'reset' | 'superseded'

export interface Resume {
  epoch: string
  client: string
  v: number
  /**
   * Counts the client's connection attempts, one more on every connection it opens. The server
   * welcomes a client id only on a higher attempt than the last it welcomed, so the hello of a
   * connection the client already gave up cannot take over from a newer one, whatever order a
   * proxy or several server processes deliver them in.
   */
  attempt: number
}

export interface HelloFrame {
  t: 'hello'
  p: 1
  presence?: true
  format: Format
  fileHash: string
  resume?: Resume
}

export interface SubmitFrame {
  t: 'submit'
  seq: number
  base: number
  ops: Json[]
}

export interface PresenceFrame {
  t: 'presence'
  v: number
  at: Json
}

export type ClientFrame =
  | HelloFrame
  | SubmitFrame
  | PresenceFrame
  | { t: 'fingerprint'; v: number; fp: string }
  | { t: 'saved'; v: number }

export type ServerFrame =
  | { t: 'need_base' }
  | { t: 'base'; bytes: number }
  | {
      t: 'welcome'
      epoch: string
      client: string
      /** the display name of this socket's principal, as the others see it in `peers` */
      name: string
      start: number
      head: number
      saved: number
      canEdit: boolean
      peers: Peer[]
      presence?: true
    }
  | ({ t: 'change' } & Change)
  | { t: 'ack'; v: number; seq: number }
  | { t: 'stale'; seq: number; base: number; head: number }
  | { t: 'saved'; v: number }
  | { t: 'peers'; peers: Peer[] }
  | (PresenceFrame & { client: string })
  | { t: 'diverged'; v: number }
  | { t: 'error'; code: ErrorCode }

/** A binary frame: always a whole base file */
export interface BytesFrame {
  t: 'bytes'
  bytes: Uint8Array
}

const ERROR_CODES: Record<ErrorCode, true> = {
  bad_frame: true,
  forbidden: true,
  too_large: true,
  schema_mismatch: true,
  reset: true,
  superseded: true,
}

type Fields = Record<string, unknown>

function invalid(): never {
  throw new Error('bad frame')
}

function fields(value: unknown): Fields {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) invalid()
  return value as Fields
}

function count(value: unknown): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) invalid()
  return value
}

function id(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0 || value.length > 256) invalid()
  return value
}

function text(value: unknown): string {
  if (typeof value !== 'string') invalid()
  return value
}

function flag(value: unknown): boolean {
  if (typeof value !== 'boolean') invalid()
  return value
}

function format(value: unknown): Format {
  const f = fields(value)
  return { name: id(f.name), schema: count(f.schema) }
}

function peers(value: unknown): Peer[] {
  if (!Array.isArray(value)) invalid()
  return value.map((entry) => {
    const f = fields(entry)
    return { client: id(f.client), name: text(f.name) }
  })
}

/** Everything JSON.parse returns is Json; only the array itself needs checking */
function ops(value: unknown): Json[] {
  if (!Array.isArray(value)) invalid()
  return value as Json[]
}

function presence(f: Fields): PresenceFrame {
  if (!Object.hasOwn(f, 'at')) invalid()
  return { t: 'presence', v: count(f.v), at: f.at as Json }
}

// Status notifications embed the selection in another object that the host must serialize safely.
function shallowPresence(at: Json, depth = 0): boolean {
  if (at === null || typeof at !== 'object') return true
  return depth < 64 && Object.values(at).every((part: Json) => shallowPresence(part, depth + 1))
}

export function withinBytes(text: string, max: number): boolean {
  return text.length <= max && (text.length * 3 <= max || utf8Length(text) <= max)
}

/** Cap the complete received and canonical frames. Only the server may add `,"client":<id>`. */
export function encodePresence(
  frame: PresenceFrame & { client?: string },
  raw?: string | Uint8Array,
): string | undefined {
  const extra =
    frame.client === undefined ? 0 : ',"client":'.length + utf8Length(JSON.stringify(frame.client))
  const max = MAX_PRESENCE_BYTES + extra
  if (raw !== undefined && (typeof raw !== 'string' || !withinBytes(raw, max))) return undefined
  try {
    const text = JSON.stringify(frame)
    if (withinBytes(text, max) && shallowPresence(frame.at)) return text
  } catch {
    // A selection from editor glue can exceed JSON's nesting limit.
  }
  return undefined
}

function hash(value: unknown): string {
  if (typeof value !== 'string' || !/^[0-9a-f]{64}$/.test(value)) invalid()
  return value
}

function json(raw: string): Fields {
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch {
    invalid()
  }
  return fields(value)
}

export function parseClientFrame(raw: string | Uint8Array): ClientFrame | BytesFrame {
  if (raw instanceof Uint8Array) return { t: 'bytes', bytes: raw }
  const f = json(raw)
  switch (f.t) {
    case 'hello': {
      if (f.p !== 1) invalid()
      const hello: HelloFrame = {
        t: 'hello',
        p: 1,
        format: format(f.format),
        fileHash: hash(f.fileHash),
        ...(f.presence === true ? { presence: true } : {}),
      }
      if (f.resume !== undefined) {
        const r = fields(f.resume)
        hello.resume = {
          epoch: id(r.epoch),
          client: id(r.client),
          v: count(r.v),
          attempt: count(r.attempt),
        }
      }
      return hello
    }
    case 'submit':
      return { t: 'submit', seq: count(f.seq), base: count(f.base), ops: ops(f.ops) }
    case 'presence':
      return presence(f)
    case 'fingerprint':
      return { t: 'fingerprint', v: count(f.v), fp: id(f.fp) }
    case 'saved':
      return { t: 'saved', v: count(f.v) }
    default:
      return invalid()
  }
}

export function parseServerFrame(raw: string | Uint8Array): ServerFrame | BytesFrame {
  if (raw instanceof Uint8Array) return { t: 'bytes', bytes: raw }
  const f = json(raw)
  switch (f.t) {
    case 'need_base':
      return { t: 'need_base' }
    case 'base':
      return { t: 'base', bytes: count(f.bytes) }
    case 'welcome':
      return {
        t: 'welcome',
        epoch: id(f.epoch),
        client: id(f.client),
        name: text(f.name),
        start: count(f.start),
        head: count(f.head),
        saved: count(f.saved),
        canEdit: flag(f.canEdit),
        peers: peers(f.peers),
        ...(f.presence === true ? { presence: true } : {}),
      }
    case 'change':
      return {
        t: 'change',
        v: count(f.v),
        client: id(f.client),
        seq: count(f.seq),
        author: text(f.author),
        ops: ops(f.ops),
      }
    case 'ack':
      return { t: 'ack', v: count(f.v), seq: count(f.seq) }
    case 'stale':
      return { t: 'stale', seq: count(f.seq), base: count(f.base), head: count(f.head) }
    case 'saved':
      return { t: 'saved', v: count(f.v) }
    case 'peers':
      return { t: 'peers', peers: peers(f.peers) }
    case 'presence':
      return { ...presence(f), client: id(f.client) }
    case 'diverged':
      return { t: 'diverged', v: count(f.v) }
    case 'error':
      if (typeof f.code !== 'string' || !Object.hasOwn(ERROR_CODES, f.code)) invalid()
      return { t: 'error', code: f.code as ErrorCode }
    default:
      return invalid()
  }
}

/** UTF-8 size of `value` without encoding it */
export function utf8Length(value: string): number {
  let size = 0
  for (let i = 0; i < value.length; i++) {
    const code = value.charCodeAt(i)
    if (code < 0x80) size += 1
    else if (code < 0x800) size += 2
    else if (code >= 0xd800 && code <= 0xdbff) {
      const next = value.charCodeAt(i + 1)
      if (next >= 0xdc00 && next <= 0xdfff) {
        size += 4
        i += 1
      } else size += 3
    } else size += 3
  }
  return size
}
