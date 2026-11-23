// @vitest-environment node
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { afterAll, beforeAll, describe, expect, it } from 'vitest'

/**
 * What `npm run build` emits must load in a browser or in Node ESM with no
 * bundler: every relative import names its file extension.
 */
const here = fileURLToPath(new URL('..', import.meta.url))
const tsc = createRequire(import.meta.url).resolve('typescript/bin/tsc')

let out = ''

beforeAll(() => {
  out = mkdtempSync(join(tmpdir(), 'seuoffice-embed-dist-'))
  execFileSync(process.execPath, [tsc, '-p', join(here, 'tsconfig.build.json'), '--outDir', out], {
    cwd: here,
    stdio: 'pipe',
  })
  // the real package is `"type": "module"`; the build output sits outside it here
  writeFileSync(join(out, 'package.json'), '{"type":"module"}')
}, 120_000)

afterAll(() => {
  rmSync(out, { recursive: true, force: true })
})

describe('the built @seuoffice/embed', () => {
  it('imports every sibling module with its extension', () => {
    expect(readFileSync(join(out, 'index.js'), 'utf8')).toMatch(/from '\.\/collab\.js'/)
    expect(readFileSync(join(out, 'index.d.ts'), 'utf8')).not.toMatch(/from '\.\/collab'/)
    expect(readFileSync(join(out, 'collab.d.ts'), 'utf8')).not.toMatch(
      /from ['"]@seuoffice\/collab/,
    )
  })

  it('loads in Node ESM without a bundler', () => {
    const script = `
      const embed = await import(${JSON.stringify(pathToFileURL(join(out, 'index.js')).href)})
      console.log(JSON.stringify({
        openDocument: typeof embed.openDocument,
        renderHistory: typeof embed.renderHistory,
        sandbox: embed.DEFAULT_SANDBOX,
        openTimeoutMs: embed.DEFAULT_OPEN_TIMEOUT_MS,
        error: typeof embed.SeuofficeEmbedError,
      }))
    `
    const printed = execFileSync(process.execPath, ['--input-type=module', '-e', script], {
      encoding: 'utf8',
    })
    expect(JSON.parse(printed)).toEqual({
      openDocument: 'function',
      renderHistory: 'function',
      sandbox: 'allow-scripts allow-same-origin allow-downloads',
      openTimeoutMs: 60000,
      error: 'function',
    })
  })
})
