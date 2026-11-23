import { readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
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

const guest = await build({
  entryPoints: ['src/guest.mjs'],
  outfile: 'dist/guest.js',
  bundle: true,
  format: 'esm',
  platform: 'browser',
  conditions: ['seuoffice-source'],
  target: 'es2022',
  metafile: true,
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
const artifact = guest.metafile.outputs['dist/guest.js']
const allowed = new Set(['node:buffer', 'node:crypto', 'node:path', 'node:util'])
for (const dep of artifact.imports) {
  if (dep.kind !== 'import-statement' || !allowed.has(dep.path))
    throw new Error(`unexpected guest import: ${dep.path}`)
}
const notices = new Map()
for (const [file, { bytesInOutput }] of Object.entries(artifact.inputs)) {
  if (!bytesInOutput || !file.includes('node_modules/')) continue
  const dir = /^.*node_modules\/(?:@[^/]+\/)?[^/]+/.exec(file)[0]
  const { name, version } = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'))
  if (notices.has(name + '@' + version)) continue
  // saxes 6 omits its upstream license file from its npm tarball.
  const names = readdirSync(dir).filter((n) =>
    /^(licen[sc]e|copying|notice|CopyrightNotice)([. -]|$)/i.test(n),
  )
  const texts = names.map((n) => readFileSync(join(dir, n), 'utf8'))
  if (name === 'saxes' && version === '6.0.0')
    texts.push(readFileSync('licenses/saxes-6.0.0.txt', 'utf8'))
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
