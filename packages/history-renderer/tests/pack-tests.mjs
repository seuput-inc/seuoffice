import { cpSync, readdirSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { build } from 'esbuild'

// Bundle the trusted fixture builders and independent reopeners, not the packages under test.
const source = fileURLToPath(new URL('.', import.meta.url))
if (!process.argv[2]) throw new Error('usage: node tests/pack-tests.mjs <consumer-test-directory>')
const outdir = resolve(process.argv[2])
await build({
  entryPoints: readdirSync(source)
    .filter((name) => name.endsWith('.test.ts'))
    .map((name) => join(source, name)),
  outdir,
  outExtension: { '.js': '.mjs' },
  format: 'esm',
  platform: 'node',
  target: 'node22',
  loader: { '.md': 'text' },
  bundle: true,
  banner: {
    js: "import {createRequire as fixtureRequire} from 'node:module'; const require = fixtureRequire(import.meta.url)",
  },
  external: [
    'vitest',
    '@seuoffice/history-renderer',
    '@seuoffice/collab/*',
    'jszip',
    'saxes',
    'pdf-lib',
  ],
})
cpSync(join(source, 'capabilities.mjs'), join(outdir, 'capabilities.mjs'))
