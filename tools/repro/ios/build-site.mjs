/**
 * `node tools/repro/ios/build-site.mjs <out>`: what the simulator job serves, in one folder:
 * dist/ (apps/web/dist, built before), embed.js (@seuoffice/embed as one classic script, as
 * apps/web/tests/cross-origin-embed.ts builds it) and files/ (one document per format).
 */
import { cpSync, mkdirSync, writeFileSync } from 'node:fs'
import { basename, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { build } from 'vite'

const repo = fileURLToPath(new URL('../../..', import.meta.url))
const out = resolve(process.argv[2] ?? 'ios-repro-site')

const FILES = [
  'fixtures/generated/simple.docx',
  'apps/docs/tests/pagination-corpus/docx/13-english-report.docx',
  'apps/sheets/fixtures/generated/compatibility-edit.xlsx',
  'packages/pptx-engine/tests/fixtures/01_standard_business.pptx',
  'apps/pdf/tests/fixtures/plain-sample.pdf',
  'apps/hwp/tests/fixtures/memo.hwp',
]

mkdirSync(resolve(out, 'files'), { recursive: true })
cpSync(resolve(repo, 'apps/web/dist'), resolve(out, 'dist'), { recursive: true })
for (const file of FILES) cpSync(resolve(repo, file), resolve(out, 'files', basename(file)))

const result = await build({
  configFile: false,
  logLevel: 'silent',
  resolve: { conditions: ['seuoffice-source'] },
  build: {
    write: false,
    minify: false,
    lib: {
      entry: resolve(repo, 'packages/embed/src/index.ts'),
      formats: ['iife'],
      name: 'SeuofficeEmbed',
    },
  },
})
writeFileSync(resolve(out, 'embed.js'), (Array.isArray(result) ? result[0] : result).output[0].code)
console.log(`site in ${out}`)
