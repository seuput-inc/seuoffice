// Drives the running app through its own bridges over the DevTools protocol
// (cdp.mjs): seeds the data a migration must keep, checks it after the
// update, and walks the update dialog as a person would.
//
//   node probe.mjs make-docx <path>             a small mock document to open
//   node probe.mjs seed   --port <p> --out <seed.json>
//   node probe.mjs check  --port <p> --seed <seed.json> [--version <v>]
//   node probe.mjs update --port <p> [--version <v>]
//   node probe.mjs dirty  --port <p> --marker <text>
//   node probe.mjs editor --port <p> --marker <text>
//   node probe.mjs version --port <p>
//
// seed: run on the old app after it opened the mock document (so the recent
// list has it). It writes a localStorage key in the shell window, an AI key
// (sealed by safeStorage on disk), and records what the app shows for both
// and for the recent list. check compares the same reads on the updated app
// and exits 1 on any difference; files on disk are compared by the calling
// script (hashes of userData).
// update: waits for the launch check to find the update, starts the
// download from Settings' entry, waits for "downloaded", presses "Restart &
// Install" in the update window.
// dirty: types the marker into the open Word document, key by key as a person does,
// and exits 1 unless the editor then holds it; the document has unsaved changes.
// editor: exits 1 unless the open Word document's editor holds the marker.

import { randomUUID } from 'node:crypto'
import { readFileSync, writeFileSync } from 'node:fs'
import { parseArgs } from 'node:util'
import { crc32 } from 'node:zlib'

import { evaluate, pageSession } from './cdp.mjs'

const [command, ...rest] = process.argv.slice(2)
const { values, positionals } = parseArgs({
  args: rest,
  allowPositionals: true,
  options: {
    port: { type: 'string', default: '9229' },
    out: { type: 'string' },
    seed: { type: 'string' },
    version: { type: 'string' },
    marker: { type: 'string' },
    timeout: { type: 'string', default: '600' },
  },
})
const port = Number(values.port)
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
const EDITOR = '.doc-page[contenteditable="true"]'
const DOCS_PAGE = 'docs/index.html'
const PROBE_KEY = 'seuoffice.migration-probe'

/** A stored (uncompressed) zip: enough for a docx Word opens */
function zip(entries) {
  const locals = []
  const centrals = []
  let offset = 0
  for (const [name, text] of entries) {
    const data = Buffer.from(text, 'utf8')
    const file = Buffer.from(name, 'utf8')
    const crc = crc32(data)
    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50, 0)
    local.writeUInt16LE(20, 4)
    local.writeUInt32LE(crc, 14)
    local.writeUInt32LE(data.length, 18)
    local.writeUInt32LE(data.length, 22)
    local.writeUInt16LE(file.length, 26)
    const central = Buffer.alloc(46)
    central.writeUInt32LE(0x02014b50, 0)
    central.writeUInt16LE(20, 4)
    central.writeUInt16LE(20, 6)
    central.writeUInt32LE(crc, 16)
    central.writeUInt32LE(data.length, 20)
    central.writeUInt32LE(data.length, 24)
    central.writeUInt16LE(file.length, 28)
    central.writeUInt32LE(offset, 42)
    locals.push(local, file, data)
    centrals.push(central, file)
    offset += local.length + file.length + data.length
  }
  const dir = Buffer.concat(centrals)
  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(entries.length, 8)
  end.writeUInt16LE(entries.length, 10)
  end.writeUInt32LE(dir.length, 12)
  end.writeUInt32LE(offset, 16)
  return Buffer.concat([...locals, dir, end])
}

const W = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
const docx = () =>
  zip([
    [
      '[Content_Types].xml',
      '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>',
    ],
    [
      '_rels/.rels',
      '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>',
    ],
    [
      'word/document.xml',
      `<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="${W}"><w:body><w:p><w:r><w:t>넘어가기 시험 모의 문서</w:t></w:r></w:p></w:body></w:document>`,
    ],
  ])

/** What the app shows for the seeded data, read the same way before and after */
const READ = `(async () => {
  const ai = await window.aiOffice.getAiSettings()
  const recents = await window.aiOffice.recents()
  return {
    version: await window.aiOffice.getAppVersion(),
    localStorage: localStorage.getItem(${JSON.stringify(PROBE_KEY)}),
    aiKey: ai?.providers?.openai?.apiKey ?? null,
    recents: recents.entries.map((e) => e.path).sort(),
  }
})()`

async function read() {
  return evaluate(port, 'aiOffice', READ)
}

/** Whether the Word editor of the open document holds the text */
function editorHolds(session, text) {
  return session.evaluate(
    `[...document.querySelectorAll(${JSON.stringify(EDITOR)})].some((e) => (e.textContent ?? '').includes(${JSON.stringify(text)}))`,
  )
}

async function waitUntil(what, check, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (await check()) return
    await sleep(500)
  }
  throw new Error(`timed out waiting for ${what}`)
}

if (command === 'make-docx') {
  writeFileSync(positionals[0], docx())
} else if (command === 'version') {
  console.log((await read()).version)
} else if (command === 'seed') {
  const token = randomUUID()
  await evaluate(
    port,
    'aiOffice',
    `(async () => {
      localStorage.setItem(${JSON.stringify(PROBE_KEY)}, ${JSON.stringify(token)})
      const s = await window.aiOffice.getAiSettings()
      s.providers.openai = { ...s.providers.openai, apiKey: ${JSON.stringify(`sk-migration-${token}`)} }
      await window.aiOffice.setAiSettings(s)
    })()`,
  )
  const seen = await read()
  const problems = []
  if (seen.localStorage !== token) problems.push('localStorage was not written')
  if (!seen.aiKey) problems.push('the AI key was not kept')
  if (seen.recents.length === 0) problems.push('the recent list is empty')
  writeFileSync(values.out, `${JSON.stringify(seen, null, 2)}\n`)
  console.log(JSON.stringify({ seeded: seen, problems }, null, 2))
  if (problems.length) process.exit(1)
} else if (command === 'check') {
  const seed = JSON.parse(readFileSync(values.seed, 'utf8'))
  const seen = await read()
  const problems = []
  if (values.version && seen.version !== values.version) {
    problems.push(`version ${seen.version}, expected ${values.version}`)
  }
  for (const key of ['localStorage', 'aiKey', 'recents']) {
    if (JSON.stringify(seen[key]) !== JSON.stringify(seed[key])) {
      problems.push(`${key}: ${JSON.stringify(seen[key])}, seeded ${JSON.stringify(seed[key])}`)
    }
  }
  console.log(JSON.stringify({ seen, problems }, null, 2))
  if (problems.length) process.exit(1)
} else if (command === 'update') {
  const deadline = Date.now() + Number(values.timeout) * 1000
  const state = () => evaluate(port, 'aiOffice', 'window.aiOffice.getUpdateState()')
  let s = await state()
  while (!s && Date.now() < deadline) {
    await sleep(5000)
    s = await state()
  }
  if (!s) throw new Error('the launch check found no update')
  if (values.version && s.version !== values.version) {
    throw new Error(`the feed offers ${s.version}, expected ${values.version}`)
  }
  await evaluate(port, 'aiOffice', 'window.aiOffice.openUpdateDialog()')
  while (s.phase !== 'downloaded' && Date.now() < deadline) {
    if (s.phase === 'error' || s.phase === 'manual') throw new Error(`update ${s.phase}`)
    await sleep(3000)
    s = await state()
  }
  if (s.phase !== 'downloaded') throw new Error(`still ${s.phase} at the deadline`)
  console.log(`downloaded ${s.version}; installing`)
  // the window closes and the app quits: the call may not answer
  await Promise.race([
    evaluate(port, 'aiOfficeUpdate', 'window.aiOfficeUpdate.install(), true').catch(() => {}),
    sleep(5000),
  ])
} else if (command === 'dirty') {
  if (!values.marker) throw new Error('dirty needs --marker')
  const session = await pageSession(port, DOCS_PAGE)
  try {
    await waitUntil(
      'the Word editor',
      () => session.evaluate(`!!document.querySelector(${JSON.stringify(EDITOR)})`),
      60_000,
    )
    // a click into the page, then the caret to its end, then the keys a person would press
    const at = await session.evaluate(`(() => {
      const editor = document.querySelector(${JSON.stringify(EDITOR)})
      editor.scrollIntoView({ block: 'start' })
      const box = editor.getBoundingClientRect()
      return { x: Math.round(box.left + 40), y: Math.round(box.top + 40) }
    })()`)
    for (const type of ['mousePressed', 'mouseReleased']) {
      await session.send('Input.dispatchMouseEvent', { type, ...at, button: 'left', clickCount: 1 })
    }
    await session.evaluate(`(() => {
      const editor = document.querySelector(${JSON.stringify(EDITOR)})
      editor.focus()
      const selection = getSelection()
      selection.selectAllChildren(editor)
      selection.collapseToEnd()
    })()`)
    for (const key of values.marker) {
      await session.send('Input.dispatchKeyEvent', { type: 'keyDown', key, text: key })
      await session.send('Input.dispatchKeyEvent', { type: 'keyUp', key })
    }
    try {
      await waitUntil(
        'the typed text in the editor',
        () => editorHolds(session, values.marker),
        15_000,
      )
    } catch (err) {
      const seen = await session.evaluate(
        `JSON.stringify({ editors: document.querySelectorAll(${JSON.stringify(EDITOR)}).length, text: (document.body.innerText ?? '').slice(0, 300) })`,
      )
      console.error(`${err.message}: ${seen}`)
      process.exit(1)
    }
    console.log(`typed ${values.marker}`)
  } finally {
    session.close()
  }
} else if (command === 'editor') {
  if (!values.marker) throw new Error('editor needs --marker')
  const session = await pageSession(port, DOCS_PAGE, 10_000)
  try {
    const holds = await editorHolds(session, values.marker)
    console.log(JSON.stringify({ holds }))
    if (!holds) process.exit(1)
  } finally {
    session.close()
  }
} else {
  console.error('usage: node probe.mjs make-docx|seed|check|update|dirty|editor|version ...')
  process.exit(2)
}
