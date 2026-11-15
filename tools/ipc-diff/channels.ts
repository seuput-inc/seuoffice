/**
 * Both canonical runtime namespaces retain their surface/registration contract.
 * Only Electron-derived rows have an Electron differential oracle.
 */
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

export interface Spec {
  name: string
  kinds: string[]
  batch: string
  surfaces: string[]
}

export interface CanonicalSpec extends Spec {
  namespace: 'electron' | 'native'
}

const file = fileURLToPath(new URL('../../crates/shell-contract/src/channels.rs', import.meta.url))
const source = readFileSync(file, 'utf8')
const table = source.slice(
  source.indexOf('pub const CHANNELS'),
  source.indexOf('pub struct PreloadSpec'),
)
const list = (text: string): string[] =>
  text
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)

export const CHANNELS: CanonicalSpec[] = [
  ...table.matchAll(
    /\b(native_spec|spec)\(\s*"([^"]+)",\s*&\[([^\]]*)\],\s*(\w+),\s*&\[([^\]]*)\],?\s*\)/g,
  ),
].map((match): CanonicalSpec => ({
  name: match[2],
  kinds: list(match[3]).map((kind) => kind[0].toLowerCase() + kind.slice(1)),
  batch: match[4],
  surfaces: list(match[5]),
  namespace: match[1] === 'native_spec' ? 'native' : 'electron',
}))

export const ELECTRON_CHANNELS = CHANNELS.filter((spec) => spec.namespace === 'electron')
export const NATIVE_CHANNELS = CHANNELS.filter((spec) => spec.namespace === 'native')

export const SPECS = new Map(CHANNELS.map((s) => [s.name, s]))

if (CHANNELS.length === 0) throw new Error(`no channel read from ${file}`)
if (new Set(CHANNELS.map((spec) => spec.name)).size !== CHANNELS.length) {
  throw new Error('duplicate canonical channel names across contract namespaces')
}
