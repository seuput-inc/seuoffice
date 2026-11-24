import { randomBytes } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { crc32, deflateRawSync } from 'node:zlib'
import JSZip from 'jszip'
import { beforeAll, describe, expect, it } from 'vitest'
import init, { HwpDocument } from '../../../vendor/rhwp-core/rhwp.js'
import { openHwp, SECTION, streamOf, writeContainer } from '../../../apps/hwp/src/main/hwp5-file'
import { newTabsOf } from '../../../apps/hwp/src/main/tab-widths'
import { openHwpFile } from '../src/hwp-file'

const fixture = (name: string) =>
  readFileSync(new URL(`../../../apps/hwp/tests/fixtures/${name}`, import.meta.url))
const blank = readFileSync(
  new URL('../../../apps/hwp/src/renderer/assets/blank.hwp', import.meta.url),
)
beforeAll(async () => {
  await init({
    module_or_path: readFileSync(
      new URL('../../../vendor/rhwp-core/rhwp_bg.wasm', import.meta.url),
    ),
  })
})

function hwpWithStream(name: string, change: (stream: Buffer) => Buffer) {
  const { cfb } = openHwp(blank)!
  const entry = cfb.FileIndex.find((entry) => entry.name === name)!
  entry.content = change(Buffer.from(entry.content as Uint8Array))
  entry.size = entry.content.length
  return writeContainer(cfb)
}

async function changedHwpx(change: (zip: JSZip) => void | Promise<void>) {
  const zip = await JSZip.loadAsync(fixture('memo.hwpx'))
  await change(zip)
  return zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
}

describe('HWP history file boundary', () => {
  it('never constructs an engine for the wrong format, unverified or protected bytes', async () => {
    let opened = 0
    const observed = new Proxy(HwpDocument, {
      construct(target, args) {
        opened++
        return Reflect.construct(target, args)
      },
    })
    const wrongCfb = readFileSync(
      new URL(
        '../../../apps/sheets/native/xlsx-engine/tests/fixtures/cell-styles.xls',
        import.meta.url,
      ),
    )
    const malformed = Buffer.from(blank)
    malformed.writeUInt32LE(malformed.readUInt32LE(44) + 1, 44)
    const large = new Uint8Array(2 * 1024 * 1024 + 1)
    large.set(blank)
    for (const bytes of [wrongCfb, malformed, fixture('memo.hwpx'), large]) {
      await expect(openHwpFile('hwp', bytes, observed)).rejects.toThrow()
    }
    for (const flag of [0x2, 0x4, 0x80, 0x4000]) {
      const protectedFile = hwpWithStream('FileHeader', (stream) => {
        stream.writeUInt32LE(stream.readUInt32LE(36) | flag, 36)
        return stream
      })
      await expect(openHwpFile('hwp', protectedFile, observed)).rejects.toThrow()
    }
    const expanded = hwpWithStream('Section0', () => deflateRawSync(Buffer.alloc(1024 * 1024 + 1)))
    await expect(openHwpFile('hwp', expanded, observed)).rejects.toThrow()
    await expect(openHwpFile('hwpx', blank, observed)).rejects.toThrow()
    const locked = new HwpDocument(blank)
    try {
      await expect(
        openHwpFile('hwpx', locked.exportHwpxWithPassword('test-only'), observed),
      ).rejects.toThrow()
    } finally {
      locked.free()
    }
    expect(opened).toBe(0)
    const accepted = await openHwpFile('hwp', blank, observed)
    accepted.doc.free()
    expect(opened).toBe(1)
  })

  it('requires the actual HWPX package, a complete spine and metered XML before parsing', async () => {
    const wrongMime = await changedHwpx((zip) => {
      zip.file('mimetype', 'application/zip')
    })
    const wrongHeader = await changedHwpx((zip) => {
      zip.file('Contents/header.xml', '<workbook/>')
    })
    const extra = await changedHwpx((zip) => {
      zip.file('Contents/vendor.bin', 'opaque metadata')
    })
    const foreign = await changedHwpx(async (zip) => {
      const xml = await zip.file('Contents/section0.xml')!.async('string')
      zip.file(
        'Contents/section0.xml',
        xml.replace('<hs:sec ', '<hs:sec xmlns:v="urn:vendor" v:note="keep" '),
      )
    })
    const missing = await changedHwpx((zip) => {
      zip.remove('Contents/section0.xml')
    })
    const oversized = await changedHwpx((zip) => {
      zip.file('Preview/PrvText.txt', 'x'.repeat(1024 * 1024 + 1))
    })
    const tracked = await changedHwpx(async (zip) => {
      const header = await zip.file('Contents/header.xml')!.async('string')
      zip.file(
        'Contents/header.xml',
        header.replace('</hh:head>', '<hh:trackChange id="1"/></hh:head>'),
      )
    })
    const noSpine = await changedHwpx(async (zip) => {
      const xml = await zip.file('Contents/content.hpf')!.async('string')
      zip.file('Contents/content.hpf', xml.replace(/<opf:itemref idref="section0"[^>]*\/>/, ''))
    })
    for (const bytes of [
      wrongMime,
      wrongHeader,
      extra,
      foreign,
      missing,
      oversized,
      tracked,
      noSpine,
    ])
      await expect(openHwpFile('hwpx', bytes, HwpDocument)).rejects.toThrow()
  })

  it.each(['hwp', 'hwpx'] as const)(
    'keeps edits and table, text-box and body memos in %s',
    async (format) => {
      const source = fixture(`memo.${format}`)
      const original = Buffer.from(source)
      const opened = await openHwpFile(format, source, HwpDocument)
      try {
        const body = JSON.parse(opened.doc.getMemoList()).find(
          (m: { place: string }) => m.place === 'body',
        )
        expect(JSON.parse(opened.doc.setMemoText(body.id, '검토 완료'))).toMatchObject({ ok: true })
        const bytes = await opened.produce()
        const back = new HwpDocument(bytes)
        try {
          expect(back.getSourceFormat()).toBe(format)
          expect(JSON.parse(back.getMemoList())).toEqual(
            expect.arrayContaining([
              expect.objectContaining({ place: 'body', text: '검토 완료' }),
              expect.objectContaining({ place: 'tableCell', text: '표 칸 메모입니다.' }),
              expect.objectContaining({ place: 'textBox', text: '글상자 메모입니다.' }),
            ]),
          )
        } finally {
          back.free()
        }
        expect(JSON.parse(opened.doc.removeMemo(body.id))).toMatchObject({ ok: true })
        const without = new HwpDocument(await opened.produce())
        try {
          const remaining = JSON.parse(without.getMemoList()) as { place: string }[]
          expect(remaining.map((m) => m.place).sort()).toEqual(['tableCell', 'textBox'])
        } finally {
          without.free()
        }
        expect(source).toEqual(original)
      } finally {
        opened.doc.free()
      }
    },
  )

  it('will not admit an unmodelled extra HWPX memo as if it were absent', async () => {
    const file = await changedHwpx(async (zip) => {
      const xml = await zip.file('Contents/section0.xml')!.async('string')
      zip.file(
        'Contents/section0.xml',
        xml.replace('</hs:sec>', '<hp:fieldBegin type="MEMO"/></hs:sec>'),
      )
    })
    await expect(openHwpFile('hwpx', file, HwpDocument)).rejects.toThrow()
  })

  it('preserves the master-page lists when rewriting the edited HWP section', async () => {
    const source = fixture('master-page-duplex-even-odd.hwp')
    const opened = await openHwpFile('hwp', source, HwpDocument)
    try {
      opened.doc.insertText(0, 0, 0, '추가 ')
      const bytes = await opened.produce()
      const section = streamOf(openHwp(bytes)!, SECTION(0))!
      for (const text of ['EKTEXT master paragraph', 'OKTEXT master paragraph'])
        expect(section.includes(Buffer.from(text, 'utf16le'))).toBe(true)
      const again = new HwpDocument(bytes)
      try {
        expect(again.getTextRange(0, 0, 0, 3)).toBe('추가 ')
      } finally {
        again.free()
      }
    } finally {
      opened.doc.free()
    }
  })

  it('measures newly typed tabs with the document rather than saving a zero width', async () => {
    const opened = await openHwpFile('hwp', blank, HwpDocument)
    try {
      opened.doc.insertText(0, 0, 0, '제목\t42')
      expect(newTabsOf(opened.doc.exportHwp(), 'hwp')).toHaveLength(1)
      expect(newTabsOf(await opened.produce(), 'hwp')).toEqual([])
    } finally {
      opened.doc.free()
    }
  })

  it('rejects an edited file larger than 2 MiB before reloading it', async () => {
    // A real picture from the blank fixture, with an incompressible ancillary PNG chunk.
    const image = openHwp(blank)!.cfb.FileIndex.find((entry) => entry.name === 'PrvImage')!
    const png = Buffer.from(image.content as Uint8Array)
    expect(png.toString('ascii', png.length - 8, png.length - 4)).toBe('IEND')
    const payload = Buffer.concat([Buffer.from('teSt'), randomBytes(2 * 1024 * 1024)])
    const chunk = Buffer.alloc(4 + payload.length + 4)
    chunk.writeUInt32BE(payload.length - 4, 0)
    payload.copy(chunk, 4)
    chunk.writeUInt32BE(crc32(payload), chunk.length - 4)
    const picture = Buffer.concat([png.subarray(0, -12), chunk, png.subarray(-12)])
    const opened = await openHwpFile('hwp', blank, HwpDocument)
    try {
      expect(
        JSON.parse(opened.doc.insertPicture(0, 0, 0, '', picture, 7200, 5400, 96, 72, 'png', ''))
          .ok,
      ).toBe(true)
      expect(opened.doc.exportHwp().length).toBeGreaterThan(2 * 1024 * 1024)
      await expect(opened.produce()).rejects.toThrow(/2 MiB/)
    } finally {
      opened.doc.free()
    }
  })
})
