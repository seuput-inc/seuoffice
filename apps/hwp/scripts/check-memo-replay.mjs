import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import init, { HwpDocument } from '../../../vendor/rhwp-core/rhwp.js'

const source = new URL('../../../vendor/rhwp-core/rhwp_bg.wasm', import.meta.url)
await init({ module_or_path: readFileSync(source) })
const blank = new HwpDocument(
  readFileSync(new URL('../src/renderer/assets/blank.hwp', import.meta.url)),
)
blank.insertText(0, 0, 0, 'body')
blank.splitParagraph(0, 0, 4)
blank.pasteHtml(0, 1, 0, '<table><tr><td>cell</td></tr></table>')
const bases = { hwp: blank.exportHwp(), hwpx: blank.exportHwpx() }
blank.free()

const unixMs = Date.parse('2024-06-30T23:58:59.321Z')
const filetime = (BigInt(unixMs) + 11644473600000n) * 10000n
const memo = (author, offsetMinutesEast) =>
  JSON.stringify({ author, text: author, authoredAt: { unixMs, offsetMinutesEast } })
const path = JSON.stringify([{ controlIndex: 0, cellIndex: 0, cellParaIndex: 0 }])
const dates = (doc) =>
  Object.fromEntries(JSON.parse(doc.getMemoList()).map(({ author, date }) => [author, date]))
const authored = { Seoul: '2024-07-01T08:58:59Z', Chicago: '2024-06-30T18:58:59Z' }

for (const [zone, local] of [
  ['UTC', '2024-06-30T23:58:59Z'],
  ['Asia/Seoul', '2024-07-01T08:58:59Z'],
]) {
  process.env.TZ = zone
  for (const [format, bytes] of Object.entries(bases)) {
    const doc = new HwpDocument(bytes)
    assert.equal(JSON.parse(doc.insertMemo(0, 0, 0, 1, memo('Seoul', 540))).ok, true)
    assert.equal(JSON.parse(doc.insertMemoByPath(0, 1, path, 0, 1, memo('Chicago', -300))).ok, true)
    assert.deepEqual(dates(doc), authored, `${format} live memos in ${zone}`)
    const saved = new HwpDocument(format === 'hwp' ? doc.exportHwp() : doc.exportHwpx())
    // HWP retains the absolute FILETIME; HWPX also carries each author's wall time.
    const expected = format === 'hwp' ? { Seoul: local, Chicago: local } : authored
    assert.deepEqual(dates(saved), expected, `${format} reopened in ${zone}`)
    for (const { author, text } of JSON.parse(saved.getMemoList())) assert.equal(text, author)
    const commands = JSON.parse(saved.getFieldList()).filter((field) => field.fieldType === 'memo')
    assert.equal(commands.length, 2)
    for (const { command } of commands) {
      const [, , , low, high] = command.split('/')
      assert.equal((BigInt(high) << 32n) | BigInt(low), filetime)
    }
    saved.free()
    doc.free()
    console.log(`${format} in ${zone}: both memo creation times survived replay and reopen`)
  }
}
