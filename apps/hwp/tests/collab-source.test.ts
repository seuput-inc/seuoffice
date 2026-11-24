import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { expect, it } from 'vitest'
import { checkHwpSourceRecords } from '../src/main/collab-source'
import { openHwp, recordsOf, rewriteStreams, SECTION, streamOf } from '../src/main/hwp5-file'

const bytes = readFileSync(join(__dirname, '../src/renderer/assets/blank.hwp'))

it('admits the template body and refuses a paragraph child that disappears on an edited save', () => {
  const opened = openHwp(bytes)!
  expect(() => checkHwpSourceRecords(opened)).not.toThrow()
  const path = SECTION(0)
  const section = streamOf(opened, path)!
  const first = recordsOf(section)!.find((r) => r.level === 0 && r.tag === 66)!
  const record = Buffer.alloc(5)
  record.writeUInt32LE((1 << 20) | (1 << 10) | 0x3fe)
  record[4] = 42
  const inserted = Buffer.concat([
    section.subarray(0, first.end),
    record,
    section.subarray(first.end),
  ])
  const file = rewriteStreams(opened, new Map([[path, inserted]]))!
  expect(() => checkHwpSourceRecords(openHwp(file)!)).toThrow(/0x3fe/)
})
