/**
 * A no-edit HWPX save keeps every element and attribute kind of the source
 * (vendor/rhwp-core/patches/0047): for each fixture, each part's (element, attribute) pairs
 * must all appear in the saved part. The line layout cache (`hp:linesegarray`) is the one
 * exception: the engine leaves it out where it cannot vouch for it and Hancom recomputes it.
 */
import { readdirSync, readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import JSZip from 'jszip'
import { JSDOM } from 'jsdom'
import { beforeAll, expect, it } from 'vitest'
import init, { HwpDocument } from '@rhwp/core'

const require = createRequire(import.meta.url)
const FIXTURES = join(__dirname, 'fixtures')
const LAYOUT_CACHE = new Set(['linesegarray', 'lineseg'])
const PART = /^Contents\/(header|section\d+)\.xml$/

beforeAll(async () => {
  const wasm = join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
})

/** `element` and `element@attribute` for every start tag of each part, by local name. */
async function kindsOf(bytes: Uint8Array): Promise<Map<string, Set<string>>> {
  const zip = await JSZip.loadAsync(bytes)
  const out = new Map<string, Set<string>>()
  for (const name of Object.keys(zip.files)) {
    if (!PART.test(name)) continue
    const xml = await zip.files[name].async('string')
    const kinds = new Set<string>()
    for (const tag of xml.matchAll(
      /<([\w.-]+:)?([\w.-]+)((?:\s+[^\s=>/]+\s*=\s*(?:"[^"]*"|'[^']*'))*)\s*\/?>/g,
    )) {
      const element = tag[2]
      if (LAYOUT_CACHE.has(element)) continue
      kinds.add(element)
      for (const attr of tag[3].matchAll(/([^\s=]+)\s*=/g)) {
        if (!attr[1].startsWith('xmlns')) kinds.add(`${element}@${attr[1].split(':').at(-1)}`)
      }
    }
    out.set(name, kinds)
  }
  return out
}

const fixtures = readdirSync(FIXTURES).filter((name) => name.endsWith('.hwpx'))

it.each(fixtures)('%s keeps its element and attribute kinds on a no-edit save', async (name) => {
  const source = new Uint8Array(readFileSync(join(FIXTURES, name)))
  const doc = new HwpDocument(source)
  let saved: Uint8Array
  try {
    saved = doc.exportHwpx()
  } finally {
    doc.free()
  }
  const before = await kindsOf(source)
  const after = await kindsOf(saved)
  const lost = [...before].flatMap(([part, kinds]) =>
    [...kinds].filter((kind) => !after.get(part)?.has(kind)).map((kind) => `${part} ${kind}`),
  )
  expect(lost).toEqual([])
})

const SECTION_XML = 'Contents/section0.xml'

/** Each table's id and its label's attributes, in document order. */
async function labelsOf(bytes: Uint8Array): Promise<[string | null, [string, string][]][]> {
  const xml = await (await JSZip.loadAsync(bytes)).files[SECTION_XML].async('string')
  const doc = new new JSDOM().window.DOMParser().parseFromString(xml, 'application/xml')
  return [...doc.getElementsByTagNameNS('*', 'label')].map((label) => [
    label.parentElement?.getAttribute('id') ?? null,
    [...label.attributes].map((a): [string, string] => [a.name, a.value]),
  ])
}

it.each(['self-closing', 'paired'])(
  'keeps a %s table label with its values on its own table',
  async (form) => {
    const zip = await JSZip.loadAsync(readFileSync(join(FIXTURES, 'title-table-zone-fill.hwpx')))
    let xml = await zip.files[SECTION_XML].async('string')
    if (form === 'paired')
      xml = xml.replace(/<hp:label ([^>]*?)\s*\/>/g, '<hp:label $1></hp:label>')
    zip.file(SECTION_XML, xml)
    const source = await zip.generateAsync({ type: 'uint8array' })
    const doc = new HwpDocument(source)
    let saved: Uint8Array
    try {
      saved = doc.exportHwpx()
    } finally {
      doc.free()
    }
    const before = await labelsOf(source)
    expect(before).toHaveLength(3)
    expect(await labelsOf(saved)).toEqual(before)
  },
)
