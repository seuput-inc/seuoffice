import { createHash } from 'node:crypto'
import { copyFileSync, readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { gzipSync } from 'node:zlib'
import { build } from 'esbuild'

await build({
  entryPoints: ['src/index.ts', 'src/sandbox.ts'],
  outdir: 'dist',
  bundle: true,
  format: 'esm',
  platform: 'neutral',
  target: 'node22',
  conditions: ['seuoffice-source'],
  mainFields: ['module', 'main'],
  external: ['node:*', '@seuoffice/collab/*', './sandbox.js'],
})

const wasm =
  process.env.SEUOFFICE_WASM ?? '../../crates/target/wasm32-wasip1/release/office_wasm.wasm'
const engine = readFileSync(wasm)
const imports = WebAssembly.Module.imports(new WebAssembly.Module(engine))
const wasi = [
  'environ_get',
  'environ_sizes_get',
  'fd_write',
  'proc_exit',
  'random_get',
  'clock_time_get',
]
for (const { module, name, kind } of imports) {
  if (
    kind !== 'function' ||
    !(
      (module === 'wasi_snapshot_preview1' && wasi.includes(name)) ||
      (module === 'seu_host' && ['host_collate', 'host_callback'].includes(name))
    )
  ) {
    throw new Error(`unexpected office engine import: ${module}.${name}`)
  }
}
// Deno's npm loader does not prepare a static .wasm import under node_modules.
// A fixed local JS module carries trusted code without granting the guest file access.
writeFileSync('dist/office-engine.js', `export default '${gzipSync(engine).toString('base64')}'\n`)
copyFileSync(
  process.env.SEUOFFICE_WASM_NOTICES ?? '../../apps/shell/build/THIRD-PARTY-NOTICES.txt',
  'dist/OFFICE-WASM-NOTICES.txt',
)

const rhwpDir = '../../vendor/rhwp-core'
const rhwp = readFileSync(join(rhwpDir, 'rhwp_bg.wasm'))
const { version, rhwp: pin } = JSON.parse(readFileSync(join(rhwpDir, 'package.json'), 'utf8'))
if (createHash('sha256').update(rhwp).digest('hex') !== pin.wasmSha256)
  throw new Error('rhwp engine does not match the pinned artifact')
writeFileSync('dist/rhwp-engine.js', `export default '${gzipSync(rhwp).toString('base64')}'\n`)

const guest = await build({
  entryPoints: [
    'src/guest.mjs',
    'src/byte-engine.mjs',
    'src/stream-guest.mjs',
    'src/mixed-guest.mjs',
  ],
  outdir: 'dist',
  bundle: true,
  format: 'esm',
  platform: 'browser',
  conditions: ['seuoffice-source'],
  target: 'es2022',
  loader: { '.md': 'text', '.otf': 'base64' },
  metafile: true,
  external: ['./office-engine.js', './rhwp-engine.js', './byte-engine.js'],
  inject: ['./src/globals.mjs'],
  plugins: [
    {
      name: 'pure-builtins',
      setup(b) {
        b.onResolve({ filter: /^node:/ }, (args) => ({
          path: args.path,
          external: true,
          sideEffects: false,
        }))
      },
    },
  ],
})
const allowed = new Set([
  'node:buffer',
  'node:crypto',
  'node:path',
  'node:util',
  'node:zlib',
  './office-engine.js',
  './rhwp-engine.js',
  './byte-engine.js',
])
const bundledInputs = new Set()
for (const artifact of Object.values(guest.metafile.outputs)) {
  for (const dep of artifact.imports) {
    if (dep.kind !== 'import-statement' || !allowed.has(dep.path))
      throw new Error(`unexpected guest import: ${dep.path}`)
  }
  for (const [file, { bytesInOutput }] of Object.entries(artifact.inputs))
    if (bytesInOutput && file.includes('node_modules/')) bundledInputs.add(file)
}
const notices = new Map([
  ['Pretendard 1.309', readFileSync('../ui/template-fonts/Pretendard-OFL.txt', 'utf8')],
  [
    '@rhwp/core@' + version,
    ['LICENSE', 'THIRD_PARTY_LICENSES.md']
      .map((n) => readFileSync(join(rhwpDir, n), 'utf8'))
      .join('\n'),
  ],
])
for (const file of bundledInputs) {
  const dir = /^.*node_modules\/(?:@[^/]+\/)?[^/]+/.exec(file)[0]
  const { name, version } = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'))
  if (notices.has(name + '@' + version)) continue
  // These npm tarballs omit the license shipped by their upstream source.
  const names = readdirSync(dir).filter((n) =>
    /^(licen[sc]e|copying|notice|CopyrightNotice)([. -]|$)/i.test(n),
  )
  const texts = names.map((n) => readFileSync(join(dir, n), 'utf8'))
  if (name === 'saxes' && version === '6.0.0')
    texts.push(readFileSync('licenses/saxes-6.0.0.txt', 'utf8'))
  if (name === '@nodable/entities' && version === '3.0.0')
    texts.push(readFileSync('licenses/nodable-entities-3.0.0.txt', 'utf8'))
  if (!texts.length) throw new Error(`missing license: ${dir}`)
  notices.set(name + '@' + version, texts.join('\n'))
}
writeFileSync(
  'dist/THIRD-PARTY-NOTICES.txt',
  [...notices]
    .sort()
    .map(([name, text]) => `${name}\n${text}`)
    .join('\n\n'),
)
