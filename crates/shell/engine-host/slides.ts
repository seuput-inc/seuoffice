import { initEngine } from '@seuoffice/engine-wasm'
import {
  buildRenderSlide,
  HeuristicMetrics,
  type FontMetrics,
  type FontMetricsProvider,
  type RunStyle,
} from '@seuoffice/pptx-render'
import type { Slide, SlideDeck } from '@seuoffice/pptx-engine'
import { listSlideTemplates } from '@seuoffice/pipelines/slides/templates'
import { PackageArchive } from '../../../packages/pptx-engine/src/zip'
import { encode } from '../bridge/wire'
import type { NativeCallback } from './transport'

type FontQuery =
  | { kind: 'metrics'; style: RunStyle }
  | { kind: 'measure'; style: RunStyle; text: string }
  | { kind: 'displayFamily'; style: RunStyle; text?: string }
  | { kind: 'substituted'; style: RunStyle }
type FontResult = FontMetrics | number | string | boolean | null
interface RenderInput {
  deck: SlideDeck
  entries: Array<{ path: string; bytes: number[] | Uint8Array }>
  originalHash: string
  fitWidthPx: number
  slideIndex?: number
  master?: Slide | null
}
interface WireResult {
  header: unknown
  buffers: string[]
}

let engineReady: Promise<void> | undefined

/** Decode jscompat::writing_for_host numbers on the native slide model boundary. */
export function reviveSlideNumbers(value: unknown): unknown {
  if (!value || typeof value !== 'object' || ArrayBuffer.isView(value)) return value
  if (Array.isArray(value)) return value.map(reviveSlideNumbers)
  const object = value as Record<string, unknown>
  if ('$num' in object && Object.keys(object).length === 1) return Number(object.$num)
  return Object.fromEntries(
    Object.entries(object).map(([key, child]) => [key, reviveSlideNumbers(child)]),
  )
}

/** Encode non-finite results with the same jscompat host marker. */
export function encodeSlideNumbers(value: unknown): unknown {
  if (typeof value === 'number' && !Number.isFinite(value)) return { $num: String(value) }
  if (!value || typeof value !== 'object' || ArrayBuffer.isView(value)) return value
  if (Array.isArray(value)) return value.map(encodeSlideNumbers)
  return Object.fromEntries(
    Object.entries(value).map(([key, child]) => [key, encodeSlideNumbers(child)]),
  )
}

function parseInput(input: unknown): RenderInput {
  if (!input || typeof input !== 'object') throw new TypeError('invalid slide render request')
  const value = input as Record<string, unknown>
  if (
    !value.deck ||
    typeof value.deck !== 'object' ||
    !('slides' in value.deck) ||
    !Array.isArray(value.deck.slides) ||
    !('size' in value.deck) ||
    !Array.isArray(value.entries) ||
    typeof value.originalHash !== 'string' ||
    typeof value.fitWidthPx !== 'number'
  )
    throw new TypeError('invalid slide render request')
  for (const entry of value.entries) {
    if (
      !entry ||
      typeof entry !== 'object' ||
      typeof entry.path !== 'string' ||
      (!(entry.bytes instanceof Uint8Array) &&
        !(
          Array.isArray(entry.bytes) &&
          entry.bytes.every(
            (n: unknown) => Number.isInteger(n) && (n as number) >= 0 && (n as number) <= 255,
          )
        ))
    ) {
      throw new TypeError('invalid slide archive entry')
    }
  }
  if (
    value.slideIndex !== undefined &&
    (!Number.isSafeInteger(value.slideIndex) || (value.slideIndex as number) < 0)
  ) {
    throw new TypeError('invalid slide index')
  }
  return value as unknown as RenderInput
}

function archiveFor(input: RenderInput): PackageArchive {
  return PackageArchive.fromWire({
    entries: input.entries.map(({ path, bytes }) => [
      path,
      bytes instanceof Uint8Array ? bytes : Uint8Array.from(bytes),
    ]),
    originalHash: input.originalHash,
  })
}

function mediaReferences(value: unknown, refs: Set<string>): void {
  if (!value || typeof value !== 'object') return
  if (Array.isArray(value)) {
    for (const item of value) mediaReferences(item, refs)
    return
  }
  for (const [key, item] of Object.entries(value)) {
    if (key === 'mediaRef' && typeof item === 'string' && item) refs.add(item)
    else mediaReferences(item, refs)
  }
}

type MediaItem = { mediaRef: string; slidePath?: string }
async function resolveMedia(
  slides: readonly { slide: Slide; path?: string }[],
  archive: PackageArchive,
  callback: NativeCallback,
  signal: AbortSignal,
): Promise<Map<string, string>> {
  const items: MediaItem[] = []
  const seen = new Set<string>()
  for (const { slide, path } of slides) {
    const refs = new Set<string>()
    mediaReferences(slide, refs)
    for (const mediaRef of refs) {
      if (!archive.readBytes(mediaRef)) continue
      const key = JSON.stringify([path ?? null, mediaRef])
      if (seen.has(key)) continue
      seen.add(key)
      items.push({ mediaRef, ...(path ? { slidePath: path } : {}) })
    }
  }
  if (!items.length) return new Map()
  signal.throwIfAborted()
  const response = await callback('slides.mediaResolve', { items })
  signal.throwIfAborted()
  if (
    !Array.isArray(response) ||
    response.length !== items.length ||
    !response.every((v: unknown) => v === null || typeof v === 'string')
  ) {
    throw new TypeError('invalid slide media resolver response')
  }
  const urls = new Map<string, string>()
  for (let i = 0; i < items.length; i++) {
    const item = items[i]!
    const url = response[i]
    if (typeof url === 'string')
      urls.set(JSON.stringify([item.slidePath ?? null, item.mediaRef]), url)
  }
  return urls
}

class DeferredFontMetrics implements FontMetricsProvider {
  private readonly fallback = new HeuristicMetrics()
  readonly pending = new Map<string, FontQuery>()
  constructor(private readonly values: Map<string, FontResult>) {}

  private lookup(query: FontQuery): FontResult | undefined {
    const snapshot = { ...query, style: { ...query.style } }
    const key = JSON.stringify(snapshot)
    if (this.values.has(key)) return this.values.get(key)
    this.pending.set(key, snapshot)
    return undefined
  }
  metrics(style: RunStyle): FontMetrics {
    const result = this.lookup({ kind: 'metrics', style })
    if (result === undefined) return this.fallback.metrics(style)
    if (
      !result ||
      typeof result !== 'object' ||
      !Number.isFinite(result.ascent) ||
      !Number.isFinite(result.descent) ||
      !Number.isFinite(result.lineHeight)
    ) {
      throw new TypeError('invalid slide font metrics response')
    }
    return result
  }
  measure(text: string, style: RunStyle): number {
    const result = this.lookup({ kind: 'measure', style, text })
    if (result === undefined) return this.fallback.measure(text, style)
    if (typeof result !== 'number' || !Number.isFinite(result))
      throw new TypeError('invalid slide font measure response')
    return result
  }
  displayFamily(style: RunStyle, text?: string): string {
    const result = this.lookup({
      kind: 'displayFamily',
      style,
      ...(text !== undefined ? { text } : {}),
    })
    if (result === undefined || result === null) return style.fontFamily
    if (typeof result !== 'string') throw new TypeError('invalid slide display family response')
    return result
  }
  substituted(style: RunStyle): boolean {
    const result = this.lookup({ kind: 'substituted', style })
    if (result === undefined) return false
    if (typeof result !== 'boolean') throw new TypeError('invalid slide font substitution response')
    return result
  }
}

export async function ensureSlidesEngine(signal: AbortSignal): Promise<void> {
  signal.throwIfAborted()
  engineReady ??= initEngine(new URL('./docs-vendor/office.wasm', document.baseURI))
  await engineReady
  signal.throwIfAborted()
}

export async function withSlidesFontMetrics<T>(
  build: (metrics: FontMetricsProvider) => T,
  callback: NativeCallback,
  signal: AbortSignal,
): Promise<T> {
  const values = new Map<string, FontResult>()
  for (;;) {
    signal.throwIfAborted()
    const metrics = new DeferredFontMetrics(values)
    const result = build(metrics)
    if (!metrics.pending.size) return result
    const pending = [...metrics.pending.entries()]
    const response = await callback('slides.fontMetrics', {
      items: pending.map(([, query]) => query),
    })
    signal.throwIfAborted()
    if (!Array.isArray(response) || response.length !== pending.length)
      throw new TypeError('invalid slide font metrics batch response')
    for (let i = 0; i < pending.length; i++) {
      if (response[i] === undefined) throw new TypeError('missing slide font metrics result')
      values.set(pending[i]![0], response[i] as FontResult)
    }
  }
}

function base64(bytes: Uint8Array): string {
  const chunks: string[] = []
  for (let i = 0; i < bytes.length; i += 0x8000)
    chunks.push(String.fromCharCode(...bytes.subarray(i, i + 0x8000)))
  return btoa(chunks.join(''))
}

async function render(
  input: unknown,
  mode: 'all' | 'slide',
  callback: NativeCallback,
  signal: AbortSignal,
): Promise<WireResult> {
  const request = parseInput(reviveSlideNumbers(input))
  await ensureSlidesEngine(signal)
  const archive = archiveFor(request)
  const selected: Array<{ slide: Slide; path?: string; slideNo?: number }> =
    mode === 'all'
      ? request.deck.slides.map((slide, i) => ({
          slide,
          ...(slide.path ? { path: slide.path } : {}),
          slideNo: i + 1,
        }))
      : request.master
        ? [{ slide: request.master }]
        : request.slideIndex !== undefined && request.deck.slides[request.slideIndex]
          ? [
              {
                slide: request.deck.slides[request.slideIndex]!,
                ...(request.deck.slides[request.slideIndex]!.path
                  ? { path: request.deck.slides[request.slideIndex]!.path }
                  : {}),
                slideNo: request.slideIndex + 1,
              },
            ]
          : []
  const media = await resolveMedia(selected, archive, callback, signal)
  const result = await withSlidesFontMetrics(
    (metrics) =>
      selected.map(({ slide, path, slideNo }) =>
        buildRenderSlide(slide, request.deck.size, {
          fitWidthPx: request.fitWidthPx,
          metrics,
          media: (ref) => media.get(JSON.stringify([path ?? null, ref])),
          ...(slideNo !== undefined ? { slideNo } : {}),
        }),
      ),
    callback,
    signal,
  )
  const frame = encode(mode === 'all' ? result : (result[0] ?? null))
  return { header: frame.header, buffers: frame.buffers.map(base64) }
}

export function renderAll(
  input: unknown,
  callback: NativeCallback,
  signal: AbortSignal,
): Promise<WireResult> {
  return render(input, 'all', callback, signal)
}

export function renderSlide(
  input: unknown,
  callback: NativeCallback,
  signal: AbortSignal,
): Promise<WireResult> {
  return render(input, 'slide', callback, signal)
}

export function templateCatalog() {
  return listSlideTemplates()
}
