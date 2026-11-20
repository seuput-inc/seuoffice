// What the real-app tests (gate=e2e) read back from the files the app wrote, independently of the
// renderer. Two readers per format:
//
//   independentText  a plain zip / XML / OLE reader written here, so the saved bytes are looked at
//                    without any of the app's own parsers
//   engineText       the repo's own parsers (packages/file-parse through the wasm engine), run in
//                    a child process under tsx so this module stays plain JavaScript
//
// Both return text; the checker looks for the marker in each.

import { spawn } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, extname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { inflateRawSync } from 'node:zlib'
import JSZip from 'jszip'
import { measurePdf } from './probe-media.mjs'

const require = createRequire(import.meta.url)
const here = dirname(fileURLToPath(import.meta.url))
export const REPO_ROOT = resolve(here, '../../..')

const ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'" }

export function decodeEntities(text) {
  return text.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (whole, name) => {
    if (name[0] === '#') {
      const code =
        name[1] === 'x' || name[1] === 'X' ? parseInt(name.slice(2), 16) : Number(name.slice(1))
      return Number.isFinite(code) ? String.fromCodePoint(code) : whole
    }
    return ENTITIES[name] ?? whole
  })
}

/**
 * The text of an XML part: a paragraph, shared string, inline string, cell or row ends a line,
 * every other tag vanishes, so a marker split over several runs of one paragraph stays whole.
 */
export function xmlText(xml) {
  return decodeEntities(
    xml.replace(/<\/(?:w:p|a:p|hp:p|si|is|c|row|text:p)>/g, '\n').replace(/<[^>]*>/g, ''),
  )
}

async function zipParts(path, pattern) {
  const zip = await JSZip.loadAsync(readFileSync(path))
  const names = Object.keys(zip.files).filter((name) => pattern.test(name))
  return Promise.all(
    names.map(async (name) => ({ name, text: await zip.file(name).async('string') })),
  )
}

async function zipXmlText(path, pattern) {
  const parts = await zipParts(path, pattern)
  if (parts.length === 0) throw new Error(`no part matching ${pattern} in ${path}`)
  return parts.map((part) => xmlText(part.text)).join('\n')
}

/** The text of the body of a HWP 5 file: every BodyText section, inflated when the header says so. */
export function hwpBinaryText(buffer) {
  const CFB = require('cfb')
  const file = CFB.read(buffer, { type: 'buffer' })
  const header = CFB.find(file, 'FileHeader')
  if (!header?.content) throw new Error('not a HWP 5 file (no FileHeader stream)')
  const flags = Buffer.from(header.content).readUInt32LE(36)
  const compressed = (flags & 1) !== 0
  if ((flags & 2) !== 0) throw new Error('password-protected HWP file')
  const sections = file.FullPaths.map((fullPath, index) => ({
    fullPath,
    entry: file.FileIndex[index],
  }))
    .filter(({ fullPath }) => /BodyText\/Section\d+$/.test(fullPath))
    .sort((a, b) => a.fullPath.localeCompare(b.fullPath, 'en', { numeric: true }))
  if (sections.length === 0) throw new Error('no BodyText section')
  return sections
    .map(({ entry }) => {
      const raw = Buffer.from(entry.content)
      return (compressed ? inflateRawSync(raw) : raw).toString('utf16le')
    })
    .join('\n')
}

/** A draw.io file's XML, with a compressed diagram (base64 of raw deflate of the URL-encoded XML) opened. */
export function drawioText(source) {
  const out = [source]
  for (const match of source.matchAll(/<diagram\b[^>]*>([\s\S]*?)<\/diagram>/g)) {
    const body = match[1].trim()
    if (!body || body.startsWith('<')) continue
    try {
      out.push(decodeURIComponent(inflateRawSync(Buffer.from(body, 'base64')).toString('latin1')))
    } catch {
      // an unreadable diagram body stays unread; the raw source above is still searched
    }
  }
  return decodeEntities(out.join('\n'))
}

/** The format a saved file is read as: its extension, lower case, without the dot. */
export const formatOf = (path) => extname(path).slice(1).toLowerCase()

/** Text read by this module alone, per format. Throws when the file cannot be read as its format. */
export async function independentText(path) {
  const format = formatOf(path)
  switch (format) {
    case 'docx':
      return zipXmlText(path, /^word\/(?:document|header\d*|footer\d*)\.xml$/)
    case 'xlsx':
      return zipXmlText(path, /^xl\/(?:sharedStrings|worksheets\/[^/]+)\.xml$/)
    case 'pptx':
      return zipXmlText(path, /^ppt\/slides\/slide\d+\.xml$/)
    case 'hwpx':
      return zipXmlText(path, /^Contents\/section\d+\.xml$/)
    case 'hwp':
      return hwpBinaryText(readFileSync(path))
    case 'xmind': {
      const parts = await zipParts(path, /^content\.json$/)
      if (parts.length === 0) throw new Error('no content.json in the mind map')
      return parts[0].text
    }
    case 'drawio':
      return drawioText(readFileSync(path, 'utf8'))
    case 'md':
    case 'html':
    case 'htm':
      return readFileSync(path, 'utf8')
    default:
      throw new Error(`no independent reader for .${format}`)
  }
}

/**
 * The page count of a PDF: the print paths' plain objects are read by probe-media's small reader,
 * and a file it cannot read (object streams) by pdf-lib.
 */
export async function pdfPageCount(path) {
  const bytes = readFileSync(path)
  try {
    return measurePdf(bytes).pages.length
  } catch (first) {
    try {
      const { PDFDocument } = await import('pdf-lib')
      return (await PDFDocument.load(bytes, { updateMetadata: false })).getPageCount()
    } catch (second) {
      throw new Error(`${first.message}; pdf-lib: ${second.message}`, { cause: second })
    }
  }
}

const ENGINE_SCRIPT = join(here, 'e2e-engine-text.mjs')

/**
 * The text the repo's own parser reads from a file, in a child process (`node --import tsx`).
 * Resolves `{ ok: true, text }`, or `{ ok: false, error }` when the parser could not run or
 * refused the file; the caller tells those two apart by `unavailable`.
 */
export function engineText(path, { timeoutMs = 180000 } = {}) {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      ['--conditions=seuoffice-source', '--import', 'tsx', ENGINE_SCRIPT, path],
      { cwd: REPO_ROOT, stdio: ['ignore', 'pipe', 'pipe'] },
    )
    let stdout = ''
    let stderr = ''
    const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs)
    child.stdout.on('data', (chunk) => (stdout += chunk))
    child.stderr.on('data', (chunk) => (stderr += chunk))
    child.on('error', (error) =>
      resolvePromise({
        ok: false,
        unavailable: true,
        error: `cannot start the parser: ${error.message}`,
      }),
    )
    child.on('close', (code) => {
      clearTimeout(timer)
      const line = stdout.trim().split('\n').pop() ?? ''
      try {
        resolvePromise(JSON.parse(line))
      } catch {
        resolvePromise({
          ok: false,
          unavailable: true,
          error: `the parser printed no result (exit ${code}): ${stderr.trim().split('\n').slice(-3).join(' | ')}`,
        })
      }
    })
  })
}
