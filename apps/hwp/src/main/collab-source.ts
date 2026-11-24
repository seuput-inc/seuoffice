import { LIST_HEADER, PARA_HEADER, SECTION, recordsOf, sectionCount, streamOf } from './hwp5-file'
import type { OpenedHwp } from './hwp5-file'

/** Check the HWP5 paragraph records rhwp would silently discard on its first edited save. */
export function checkHwpSourceRecords(opened: OpenedHwp): void {
  let remaining = 262144
  const sections = sectionCount(opened)
  if (!sections) throw new Error('HWP has no readable body section')
  for (let i = 0; i < sections; i++) {
    const bytes = streamOf(opened, SECTION(i))
    const records = bytes && recordsOf(bytes, remaining)
    if (!records?.length) throw new Error('HWP section exceeds the supported record profile')
    remaining -= records.length
    let lastBody = records.length - 1
    while (lastBody >= 0) {
      const { tag, level } = records[lastBody]!
      if (level === 0 && tag === PARA_HEADER) break
      lastBody--
    }
    if (lastBody === -1) throw new Error('HWP section has no body paragraph')
    const parents: number[] = []
    for (let j = 0; j < records.length; j++) {
      const { tag, level } = records[j]!
      // A level-one master page or memo list after the body has its own raw-record owner.
      if (j > lastBody && level === 1 && (tag === LIST_HEADER || tag === 93)) break
      if (level > parents.length || (level === 0 && tag !== PARA_HEADER)) {
        throw new Error('HWP contains a record without a supported owner')
      }
      if (level && parents[level - 1] === PARA_HEADER && (tag < 67 || tag > 71)) {
        throw new Error(`HWP paragraph contains unsupported record 0x${tag.toString(16)}`)
      }
      parents.length = level
      parents.push(tag)
    }
  }
}
