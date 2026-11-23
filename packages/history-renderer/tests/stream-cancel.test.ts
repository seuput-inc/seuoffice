import { createHash } from 'node:crypto'
import { setImmediate } from 'node:timers/promises'
import { renderStream } from '@seuoffice/history-renderer'
import { PDFDocument, PDFName } from 'pdf-lib'
import { expect, it, vi } from 'vitest'

function barrier() {
  let release!: () => void
  const waiting = new Promise<void>((resolve) => {
    release = resolve
  })
  return { waiting, release }
}

function header(bytes: Uint8Array) {
  return Buffer.from(
    JSON.stringify({
      epoch: 'terrain',
      version: 0,
      format: { name: 'pdf', schema: 2 },
      baseHash: createHash('sha256').update(bytes).digest('hex'),
    }),
  )
}

async function* changes(): AsyncGenerator<Uint8Array> {}

it('discards an already cancelled stage without opening the source', async () => {
  const signal = AbortSignal.abort()
  const source = { byteLength: 1, open: vi.fn(changes) }
  const stage = { abort: vi.fn(async () => {}), write: vi.fn(async () => {}) }
  await expect(
    renderStream(source, header(Buffer.alloc(1)), changes(), stage, signal),
  ).rejects.toBe(signal.reason)
  expect(source.open).not.toHaveBeenCalled()
  expect(stage.write).not.toHaveBeenCalled()
  expect(stage.abort).toHaveBeenCalledOnce()
})

it('cancels the live guest without waiting for a blocked source', async () => {
  const controller = new AbortController()
  const reading = barrier()
  const finish = vi.fn(async () => ({ done: true as const, value: undefined }))
  const source = {
    byteLength: 1,
    open: () => ({
      [Symbol.asyncIterator]: () => ({
        next() {
          reading.release()
          return new Promise<IteratorResult<Uint8Array>>(() => {})
        },
        return: finish,
      }),
    }),
  }
  const stage = { abort: vi.fn(async () => {}), write: vi.fn(async () => {}) }
  const rendering = renderStream(
    source,
    header(Buffer.alloc(1)),
    changes(),
    stage,
    controller.signal,
  )
  await Promise.race([reading.waiting, rendering])
  controller.abort()
  await expect(rendering).rejects.toBe(controller.signal.reason)
  expect(finish).toHaveBeenCalledOnce()
  expect(stage.abort).toHaveBeenCalledOnce()
  expect(stage.write).not.toHaveBeenCalled()
})

it('discards partial output without waiting for a write and never starts another', async () => {
  const pdf = await PDFDocument.create({ updateMetadata: false })
  const page = pdf.addPage([1, 1])
  const image = pdf.context.register(
    pdf.context.stream(new Uint8Array(3), {
      Type: 'XObject',
      Subtype: 'Image',
      Width: 1,
      Height: 1,
      ColorSpace: 'DeviceRGB',
      BitsPerComponent: 8,
    }),
  )
  page.node.set(PDFName.Resources, pdf.context.obj({ XObject: { Map: image } }))
  page.node.set(
    PDFName.Contents,
    pdf.context.register(pdf.context.stream('q 1 0 0 1 0 0 cm /Map Do Q')),
  )
  const bytes = await pdf.save({ useObjectStreams: false })
  const controller = new AbortController()
  const writing = barrier()
  const completion = barrier()
  const source = {
    byteLength: bytes.length,
    async *open() {
      yield bytes.subarray(0, 100)
      yield bytes.subarray(100)
    },
  }
  let retained = 0
  const stage = {
    write: vi.fn(async (chunk: Uint8Array) => {
      retained += chunk.length
      writing.release()
      await completion.waiting
    }),
    async abort() {
      retained = 0
    },
  }
  const rendering = renderStream(source, header(bytes), changes(), stage, controller.signal)
  await Promise.race([writing.waiting, rendering])
  expect(retained).toBeGreaterThan(0)
  controller.abort()
  try {
    await expect(rendering).rejects.toBe(controller.signal.reason)
    expect(retained).toBe(0)
  } finally {
    completion.release()
  }
  await setImmediate()
  expect(stage.write).toHaveBeenCalledOnce()
})
