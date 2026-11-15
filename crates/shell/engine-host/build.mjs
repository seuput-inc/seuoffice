import { build } from 'esbuild'
import { cp, mkdir, readFile } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const root = resolve(here, '../../..')
const outdir = resolve(process.argv[2] ?? resolve(here, '../out/engine-host'))
const pdfSource = resolve(root, 'packages/file-parse/src/pdf.ts')
const require = createRequire(resolve(root, 'packages/file-parse/package.json'))
const pdfPath = require.resolve('pdfjs-dist/legacy/build/pdf.mjs')
const hostSteps = resolve(root, 'packages/file-parse/src/host-steps.ts')
const markup = resolve(here, 'markup.ts')
const pdfRequire = createRequire(resolve(root, 'apps/pdf/package.json'))

const browserPdf = {
  name: 'browser-pdf-font-path',
  setup(builder) {
    builder.onLoad(
      { filter: /[/\\]packages[/\\]file-parse[/\\]src[/\\]pdf\.ts$/ },
      async ({ path }) => {
        if (path !== pdfSource) throw new Error(`Unexpected PDF source: ${path}`)
        let source = await readFile(path, 'utf8')
        const imports =
          "import { createRequire } from 'node:module'\nimport { dirname, join } from 'node:path'\n"
        const fontFunction = /function standardFontDataUrl\(\): string \| undefined \{[\s\S]*?\n\}/
        if (!source.includes(imports) || !fontFunction.test(source))
          throw new Error('PDF font resolver changed')
        source = source
          .replace(imports, '')
          .replace(
            fontFunction,
            "function standardFontDataUrl(): string { return new URL('./standard_fonts/', document.baseURI).href }",
          )
        return { contents: source, loader: 'ts', resolveDir: dirname(path) }
      },
    )
  },
}
const browserMarkup = {
  name: 'browser-markup',
  setup(builder) {
    builder.onResolve({ filter: /^\.\/diagram$/ }, ({ importer }) =>
      importer === hostSteps ? { path: markup } : undefined,
    )
    builder.onLoad(
      { filter: /[/\\]packages[/\\]file-parse[/\\]src[/\\]host-steps\.ts$/ },
      async ({ path }) => {
        if (path !== hostSteps) throw new Error(`Unexpected host steps source: ${path}`)
        const source = await readFile(path, 'utf8')
        return {
          contents: `import { Buffer } from 'buffer'\n${source}`,
          loader: 'ts',
          resolveDir: dirname(path),
        }
      },
    )
  },
}
const rawScripts = {
  name: 'html-converter-browser-scripts',
  setup(builder) {
    builder.onResolve({ filter: /\?raw$/ }, ({ path, importer }) => ({
      path: resolve(dirname(importer), path.slice(0, -4)),
      namespace: 'converter-raw-script',
    }))
    builder.onLoad({ filter: /.*/, namespace: 'converter-raw-script' }, async ({ path }) => ({
      contents: await readFile(path, 'utf8'),
      loader: 'text',
    }))
  },
}
const browserZlib = {
  name: 'document-browser-zlib',
  setup(builder) {
    const callers = new Set([
      resolve(root, 'packages/doc2docx/src/drawings.ts'),
      resolve(root, 'packages/html2docx/src/generate/page-settings.ts'),
      resolve(root, 'packages/pdf2docx/src/extract/png.ts'),
    ])
    builder.onResolve({ filter: /^node:zlib$/ }, ({ importer }) =>
      callers.has(importer) ? { path: resolve(here, 'zlib.ts') } : undefined,
    )
  },
}
const browserQpdf = {
  name: 'qpdf-browser-runtime',
  setup(builder) {
    const qpdf = require.resolve('@neslinesli93/qpdf-wasm')
    builder.onLoad({ filter: /[/\\]qpdf-wasm[/\\]dist[/\\]qpdf\.js$/ }, async ({ path }) => {
      if (path !== qpdf) throw new Error(`Unexpected qpdf source: ${path}`)
      const source = await readFile(path, 'utf8')
      const nodeBranch = /if\(fa\)\{var fs=require\("fs"\);[\s\S]*?\}else if\(da\|\|ea\)/
      if (!nodeBranch.test(source)) throw new Error('qpdf environment selection changed')
      return {
        contents: source.replace(nodeBranch, 'if(da||ea)'),
        loader: 'js',
        resolveDir: dirname(path),
      }
    })
  },
}

await mkdir(outdir, { recursive: true })
await build({
  entryPoints: {
    main: resolve(here, 'main.ts'),
    'legacy-doc-worker': resolve(here, 'legacy-doc-worker.ts'),
    'diagram-open-worker': resolve(here, 'diagram-open-worker.ts'),
    'docs-vendor/hwp-worker': resolve(here, 'docs-vendor/hwp-worker.ts'),
    'docs-vendor/html-worker': resolve(here, 'docs-vendor/html-worker.ts'),
    'pdf/page-worker': resolve(here, 'pdf/page-worker.ts'),
    'pdf/structure-worker': resolve(here, 'pdf/structure-worker.ts'),
    'pdf/qpdf-worker': resolve(here, 'pdf/qpdf-worker.ts'),
    'pdf/pdfium-session/worker': resolve(here, 'pdf/pdfium-session/worker.ts'),
    'pdf/font-subset-worker': resolve(here, 'pdf/font-subset-worker.ts'),
    'pdf/document-edit-primitives/worker': resolve(here, 'pdf/document-edit-primitives/worker.ts'),
  },
  outdir,
  bundle: true,
  platform: 'browser',
  format: 'esm',
  target: 'chrome120',
  sourcemap: true,
  plugins: [browserPdf, browserMarkup, rawScripts, browserZlib, browserQpdf],
  inject: [resolve(here, 'buffer-globals.ts')],
})
await cp(resolve(here, 'index.html'), resolve(outdir, 'index.html'))
await cp(resolve(dirname(pdfPath), '../../standard_fonts'), resolve(outdir, 'standard_fonts'), {
  recursive: true,
})
await mkdir(resolve(outdir, 'docs-vendor'), { recursive: true })
await cp(require.resolve('@rhwp/core/rhwp_bg.wasm'), resolve(outdir, 'docs-vendor/rhwp.wasm'))
const officeWasm = process.env.SEUOFFICE_WASM
  ? resolve(process.env.SEUOFFICE_WASM)
  : resolve(root, 'crates/target/wasm32-wasip1/release/office_wasm.wasm')
await cp(officeWasm, resolve(outdir, 'docs-vendor/office.wasm'))
await mkdir(resolve(outdir, 'pdf'), { recursive: true })
const pdfiumWasm = require.resolve('@embedpdf/pdfium/pdfium.wasm')
await cp(pdfiumWasm, resolve(outdir, 'pdf/pdfium.wasm'))
await cp(
  require.resolve('@neslinesli93/qpdf-wasm/dist/qpdf.wasm'),
  resolve(outdir, 'pdf/qpdf.wasm'),
)
await cp(pdfRequire.resolve('harfbuzzjs/hb-subset.wasm'), resolve(outdir, 'pdf/hb-subset.wasm'))
