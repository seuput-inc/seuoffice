import { createHash, webcrypto } from 'node:crypto'
import { readFile, readdir } from 'node:fs/promises'
import { resolve } from 'node:path'
import JSZip from 'jszip'
import { convertPptToPptx } from '../../../../../packages/ppt2pptx/src/index'
import {
  buildPptStreams,
  buildPptContainer,
  type BuildOptions,
  type SlideSpec,
} from '../../../../../packages/ppt2pptx/tests/synthetic-ppt'
import { registerHooks } from 'node:module'
import {
  parseOdfXml,
  nodeName,
  nodeChildren,
  nodeAttrs,
  isTextNode,
  textOfNode,
  type XNode,
} from '../../../../../packages/odf2ooxml/src/xml'
import { odfLengthToEmu } from '../../../../../packages/odf2ooxml/src/units'

const binding = new URL('../../../../../packages/pptx-engine/src/index', import.meta.url).href
const reference = new URL('../../../../../tools/pptx-diff/reference/index.ts', import.meta.url).href
let referenceRoutes = 0
registerHooks({
  resolve(specifier, context, nextResolve) {
    const target =
      context.parentURL && specifier.startsWith('.')
        ? new URL(specifier, context.parentURL).href.replace(/\.ts$/, '')
        : ''
    if (target === binding) {
      referenceRoutes++
      return nextResolve(reference, context)
    }
    return nextResolve(specifier, context)
  },
})
const { convertOdpToPptx } = await import('../../../../../packages/odf2ooxml/src/odp-pptx')
if (!globalThis.crypto) Object.defineProperty(globalThis, 'crypto', { value: webcrypto })
const b64 = (bytes: Uint8Array) => Buffer.from(bytes).toString('base64')
const error = (value: unknown) => (value instanceof Error ? value.message : String(value))
// Only random identity values are normalized. Structure, relationships, run
// segmentation, geometry, media bytes and all other XML are compared verbatim.
const stable = (xml: string) =>
  xml.replace(
    /(<a16:creationId\b[^>]*\bid=")\{[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\}(")/gi,
    (_match, before: string, after: string) => `${before}{UUID}${after}`,
  )
async function parts(bytes: Uint8Array, legacy: boolean) {
  const zip = await JSZip.loadAsync(bytes)
  const out: Record<string, string> = {}
  for (const name of Object.keys(zip.files).sort()) {
    if (zip.files[name].dir) continue
    if (!legacy && !/^ppt\/(?:presentation\.xml|slides\/|media\/)/.test(name)) continue
    const data = await zip.files[name].async('uint8array')
    out[name] = /\.(?:xml|rels)$/.test(name) ? stable(new TextDecoder().decode(data)) : b64(data)
  }
  return out
}
const cases: unknown[] = []
async function ppt(
  name: string,
  specs: SlideSpec[],
  options: BuildOptions = {},
  change?: (streams: { document: Uint8Array; currentUser: Uint8Array }) => void,
) {
  const streams = buildPptStreams(specs, options)
  change?.(streams)
  const input = buildPptContainer(streams.document, streams.currentUser)
  let expected: unknown
  try {
    const result = await convertPptToPptx(input)
    expected = {
      slides: result.slideCount,
      warnings: result.warnings,
      dropped: [],
      parts: await parts(result.bytes, true),
    }
  } catch (failure) {
    expected = { error: error(failure) }
  }
  cases.push({ name, legacy: true, input: b64(input), expected })
}
const title = (text: string) => ({
  textSequences: [
    { kind: 0, text },
    { kind: 4, text: 'body\r\vnext\x1eend\x1f' },
  ],
})
await ppt('ppt-unicode-controls', [
  title('한글 😀 e\u0301 <&>\u0000\ufffe'),
  { textSequences: [{ kind: 6, text: '\x91quotes\x92\x93wide\x94\x95\x96\x97', atom: 'bytes' }] },
])
await ppt('ppt-outline', [title('outline')], { outlineTexts: true, slideSize: [1234, 4321] })
await ppt('ppt-newest-generation', [title('STALE')], {
  secondGeneration: [title('current'), title('last')],
  outlineTexts: true,
})
await ppt('ppt-duplicate-aliases', [title('once')], {
  slideListRepeats: 5,
  aliasRepeats: true,
  outlineTexts: true,
})
await ppt('ppt-overlapping-containers', [title('overlap')], { nestSlides: 3 })
await ppt('ppt-encrypted', [title('secret')], { encrypted: true })
await ppt('ppt-no-current-user', [title('flat')], {}, (streams) => {
  streams.currentUser = new Uint8Array()
})
await ppt('ppt-bad-live-edit', [title('bad')], {}, (streams) => {
  new DataView(streams.currentUser.buffer).setUint32(16, streams.document.length - 2, true)
})
await ppt('ppt-empty-deck', [])
for (const input of [new Uint8Array(), new Uint8Array([1, 2, 3]), new Uint8Array(512)]) {
  try {
    await convertPptToPptx(input)
    throw new Error('unexpected accepted invalid fixture')
  } catch (failure) {
    cases.push({
      name: `ppt-invalid-${input.length}`,
      legacy: true,
      input: b64(input),
      expected: { error: error(failure) },
    })
  }
}

const ns =
  'xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"'
const content = (body: string, styles = '') =>
  `<office:document-content ${ns}>${styles}<office:body><office:presentation>${body}</office:presentation></office:body></office:document-content>`
const page = (body: string) => `<draw:page>${body}</draw:page>`
const frame = (
  body: string,
  geometry = 'svg:x="1cm" svg:y="-0.25in" svg:width="10cm" svg:height="0pt"',
) => `<draw:frame ${geometry}>${body}</draw:frame>`
const textbox = (text: string) => `<draw:text-box>${text}</draw:text-box>`
const png =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=='
async function odp(
  name: string,
  xml: string,
  styles?: string,
  files: Record<string, Uint8Array | string> = {},
  template = false,
) {
  const zip = new JSZip()
  zip.file(
    'mimetype',
    `application/vnd.oasis.opendocument.presentation${template ? '-template' : ''}`,
  )
  zip.file('content.xml', xml)
  if (styles !== undefined) zip.file('styles.xml', styles)
  for (const [path, bytes] of Object.entries(files)) zip.file(path, bytes)
  const input = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
  let expected: unknown
  try {
    const result = await convertOdpToPptx(input)
    expected = {
      slides: result.slides,
      warnings: [],
      dropped: result.dropped.map(({ what, count }) => [what, count]),
      parts: await parts(result.pptx, false),
    }
  } catch (failure) {
    expected = { error: error(failure) }
  }
  cases.push({ name, legacy: false, input: b64(input), expected })
}
await odp(
  'odp-unicode-mixed-list',
  content(
    page(
      frame(
        textbox(
          '<text:p>한글 😀  é<text:tab/><text:s text:c="3.9"/><text:line-break/>after &#65; &amp; <![CDATA[ignored]]></text:p><text:list><text:list-item><text:p>one</text:p><text:list><text:list-item><text:p>two</text:p></text:list-item></text:list></text:list-item></text:list>',
        ),
      ),
    ),
  ),
)
const named = `<office:document-styles ${ns}><office:font-face-decls><style:font-face style:name="face" svg:font-family="'Noto Sans'"/></office:font-face-decls><office:styles><style:default-style style:family="paragraph"><style:text-properties fo:font-size="12pt" fo:color="#abc"/><style:paragraph-properties fo:margin-top="-1pt"/></style:default-style><style:style style:name="parent" style:family="paragraph"><style:text-properties fo:font-weight="bold" style:text-underline-type="single" style:text-transform="uppercase" style:font-name="face"/><style:paragraph-properties fo:text-align="center" fo:margin-bottom="2pt"/></style:style></office:styles><office:automatic-styles><style:page-layout style:name="L"><style:page-layout-properties fo:page-width="25.4cm" fo:page-height="19.05cm"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:page-layout-name="L"/></office:master-styles></office:document-styles>`
const auto =
  '<office:automatic-styles><style:style style:name="child" style:family="paragraph" style:parent-style-name="parent"><style:text-properties style:text-underline-type="none" style:text-transform="lowercase"/><style:paragraph-properties fo:text-align="end"/></style:style><style:style style:name="s" style:family="text"><style:text-properties fo:font-style="italic" fo:background-color="#123" style:text-position="super 58%"/></style:style></office:automatic-styles>'
await odp(
  'otp-inherited-styles-reset',
  content(
    page(
      frame(
        textbox(
          '<text:p text:style-name="child">base<text:span text:style-name="s">span</text:span><text:a xlink:href="https://example.invalid/a">link</text:a>tail</text:p>',
        ),
        'presentation:class="title" svg:width="2cm"',
      ),
    ),
    auto,
  ),
  named,
  {},
  true,
)
await odp(
  'odp-images-order-and-uri',
  content(
    page(
      frame('<draw:image xlink:href="./Pictures/a%20b.png"/>') +
        frame(textbox('<text:p>above ZIP image</text:p>')) +
        frame(`<draw:image><office:binary-data>${png}</office:binary-data></draw:image>`) +
        frame('<draw:image xlink:href="missing.bin"/>') +
        frame('<draw:image><office:binary-data>!!!</office:binary-data></draw:image>'),
    ),
  ),
  undefined,
  { 'Pictures/a b.png': Buffer.from(png, 'base64') },
)
await odp(
  'odp-drops-and-inline-frames',
  content(
    page(
      frame(
        textbox(
          '<text:p>kept<text:note>footnote</text:note><office:annotation>comment</office:annotation>' +
            frame('<draw:image xlink:href="x"/>') +
            frame('<draw:image xlink:href="y"/>') +
            frame(textbox('<text:p>flatten</text:p>')) +
            '<draw:object/><text:page-number>9</text:page-number></text:p>',
        ),
      ) +
        frame('<table:table/>') +
        frame('') +
        '<draw:frame/><draw:frame presentation:class="notes"/><presentation:notes/><draw:rect/>',
    ),
  ),
)
await odp('odp-zero-pages', content(''))
await odp(
  'odp-two-pages',
  content(
    page(frame(textbox('<text:p>one</text:p>'))) + page(frame(textbox('<text:p>two</text:p>'))),
  ),
)
await odp(
  'odp-malformed-optional-styles',
  content(page(frame(textbox('<text:p>still readable</text:p>')))),
  '<a><!DOCTYPE broken',
)
await odp(
  'odp-plain-dtd-entities',
  '<!DOCTYPE office:document-content [<!ENTITY label "bonjour">]>' +
    content(page(frame(textbox('<text:p>&label; &amp; &#0; &#xD800; &#65;</text:p>')))),
)
await odp(
  'odp-blocked-entity',
  '<!DOCTYPE office:document-content [<!ENTITY bad "javascript:alert(1)">]>' +
    content(page(frame(textbox('<text:p>&bad;</text:p>')))),
)
await odp(
  'odp-external-entity-rejected',
  '<!DOCTYPE office:document-content [<!ENTITY bad SYSTEM "file:///etc/passwd">]>' + content(''),
)
await odp(
  'odp-not-presentation',
  '<office:document><office:body><office:text/></office:body></office:document>',
)
await odp('odp-malformed-uri', content(page(frame('<draw:image xlink:href="%ZZ.png"/>'))))
await odp('odp-bad-content', '<a><![CDATA[unclosed')
await odp(
  'odp-inherited-default-box-key',
  content(
    page(frame(textbox('<text:p>prototype box</text:p>'), 'presentation:class="constructor"')),
  ),
)
await odp(
  'odp-exact-malformed-percent-path',
  content(page(frame('<draw:image xlink:href="%ZZ.png"/>'))),
  undefined,
  { '%ZZ.png': Buffer.from(png, 'base64') },
)
const large = new Uint8Array(64 * 1024 * 1024 + 1)
await odp('odp-unused-part-over-64mib', content(''), undefined, { 'unused.bin': large })
await odp(
  'odp-used-image-over-64mib',
  content(page(frame('<draw:image xlink:href="large.png"/>'))),
  undefined,
  { 'large.png': large },
)
await odp('odp-styles-part-over-64mib', content(''), undefined, { 'styles.xml': large })
const paddedPng = new Uint8Array(Buffer.from(png, 'base64').length + 96 * 1024)
paddedPng.set(Buffer.from(png, 'base64'))
let randomState = 0x12345678
for (let index = Buffer.from(png, 'base64').length; index < paddedPng.length; index++) {
  randomState ^= randomState << 13
  randomState ^= randomState >>> 17
  randomState ^= randomState << 5
  paddedPng[index] = randomState & 255
}
await odp(
  'odp-zip-image-completion-order',
  content(
    page(
      frame(
        '<draw:image xlink:href="Pictures/large.png"/>',
        'svg:x="1cm" svg:y="1cm" svg:width="3cm" svg:height="3cm"',
      ) +
        frame(
          '<draw:image xlink:href="Pictures/small.png"/>',
          'svg:x="8cm" svg:y="1cm" svg:width="3cm" svg:height="3cm"',
        ),
    ),
  ),
  undefined,
  { 'Pictures/large.png': paddedPng, 'Pictures/small.png': Buffer.from(png, 'base64') },
)

const xmlInputs = [
  '<a>&#65;&#x1f600;&amp;</a>',
  '<!DOCTYPE a><a>ok</a>',
  '<!DOCTYPE a [<!ENTITY x "bonjour">]><a>&x;</a>',
  '<!DOCTYPE a [<!ENTITY x "&amp;">]><a>&x;</a>',
  '<a><![CDATA[a&b]]>c</a>',
  '<a>&#xD800;&#0;&#1;&#9;</a>',
  '<?xml version="1.1"?><a>&#1;</a>',
  '<a><!--inside-->before<?other x="y"?>after</a>',
  '<a>unclosed',
  '<a></wrong>',
  '<a><constructor/></a>',
  '<!DOCTYPE a [<!ENTITY x "javascript:alert(1)">]><a>&x;</a>',
]
xmlInputs.push(
  '<a>before<!--ignored-->after</a>',
  '<a x="&amp;#65;">&amp;#65;</a>',
  '<!DOCTYPE a [<!ENTITY x "good"><!ENTITY x "javascript:alert(1)">]><a>&x;</a>',
)
xmlInputs.push('<a/></a><?late?>', '<a/></a> <?late?>')
interface ProjectedNode {
  text?: string
  name?: string | null
  attrs?: Record<string, string>
  children?: ProjectedNode[]
}
const tree = (nodes: XNode[]): ProjectedNode[] =>
  nodes.flatMap((node) =>
    isTextNode(node)
      ? [{ text: textOfNode(node) }]
      : nodeName(node)
        ? [{ name: nodeName(node), attrs: nodeAttrs(node), children: tree(nodeChildren(node)) }]
        : [],
  )
const xml = xmlInputs.map((input) => {
  const nodes = parseOdfXml(input)
  return { input, expected: nodes ? tree(nodes) : null }
})
const unitInputs = [
  '0',
  '-0',
  '1',
  '+1cm',
  '.5in',
  '-0.00001pt',
  '2pc',
  ' 3px ',
  '1CM',
  '1e3pt',
  '100%',
  '2777.777777cm',
  '2777.77778cm',
  '0pt',
  'NaNpt',
  '1.2.3cm',
  '\u00a01cm\ufeff',
]
const units = unitInputs.map((input) => ({ input, expected: odfLengthToEmu(input) }))
const sources = [
  'packages/ppt2pptx/src/index.ts',
  'packages/ppt2pptx/src/container.ts',
  'packages/ppt2pptx/src/ppt.ts',
  'packages/ppt2pptx/src/pptx-writer.ts',
  'packages/ppt2pptx/tests/synthetic-ppt.ts',
  'packages/odf2ooxml/src/odp-pptx.ts',
  'packages/odf2ooxml/src/odf.ts',
  'packages/odf2ooxml/src/text.ts',
  'packages/odf2ooxml/src/styles.ts',
  'packages/odf2ooxml/src/xml.ts',
  'packages/odf2ooxml/src/units.ts',
]
for (const file of await readdir(resolve('tools/pptx-diff/reference'), { recursive: true })) {
  if (/\.(?:ts|js|mjs|json)$/.test(file)) sources.push(`tools/pptx-diff/reference/${file}`)
}
sources.push(
  'tools/pptx-diff/reference/README.md',
  'node_modules/fast-xml-parser/package.json',
  'node_modules/@nodable/entities/package.json',
  'node_modules/is-unsafe/package.json',
  'node_modules/jszip/package.json',
)
sources.push('node_modules/jszip/lib/stream/DataWorker.js', 'node_modules/jszip/lib/flate.js')
sources.push('crates/main-core/src/slides/legacy_presentation/reference.mts')
const hashes = Object.fromEntries(
  await Promise.all(
    sources.map(async (path) => [
      path,
      createHash('sha256')
        .update(await readFile(resolve(path)))
        .digest('hex'),
    ]),
  ),
)
if (referenceRoutes === 0)
  throw new Error('ODP oracle must use the frozen TypeScript engine, never the Rust/WASM binding')
console.log(
  JSON.stringify(
    {
      provenance: {
        runtime: process.versions,
        hashes,
        engine: 'tools/pptx-diff/reference at 8defaf3da7114da5ae1a332fded0a4b2676d2b4d',
        volatileNormalization:
          'a16:creationId id UUID only; extension URI and all other values retained',
      },
      cases,
      xml,
      units,
    },
    (_key, value: unknown) => {
      if (typeof value !== 'string' || value.length <= 4096) return value
      const chunks = value.match(/[\s\S]{1,4096}/gu)
      if (!chunks) throw new Error('a nonempty fixture string must have chunks')
      return chunks
    },
  ),
)
