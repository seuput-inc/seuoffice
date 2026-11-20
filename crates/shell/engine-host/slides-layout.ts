import { buildRenderSlide, EMU_PER_PX_96, layoutText, makeViewport } from '@seuoffice/pptx-render'
import type { Slide, SlideDeck, TextBody } from '@seuoffice/pptx-engine'
import type { NativeCallback } from './transport'
import {
  encodeSlideNumbers,
  ensureSlidesEngine,
  reviveSlideNumbers,
  withSlidesFontMetrics,
} from './slides'

interface MeasurementInput {
  slide: Slide
  size: SlideDeck['size']
}

/** Renderer-only half of growTextBoxesToContent; the native caller owns mutations. */
export async function measurePageText(
  input: unknown,
  callback: NativeCallback,
  signal: AbortSignal,
) {
  input = reviveSlideNumbers(input)
  if (!input || typeof input !== 'object' || !('slide' in input) || !('size' in input)) {
    throw new TypeError('invalid slide text measurement input')
  }
  const { slide, size } = input as MeasurementInput
  await ensureSlidesEngine(signal)
  const rendered = await withSlidesFontMetrics(
    (metrics) =>
      buildRenderSlide(slide, size, {
        fitWidthPx: size.cx / EMU_PER_PX_96,
        metrics,
      }),
    callback,
    signal,
  )
  return encodeSlideNumbers(
    rendered.nodes.flatMap((node) => {
      if (node.type !== 'text' || !node.text) return []
      const text = node.text
      return [
        {
          sourceId: node.sourceId,
          boxHeight: node.box.h,
          contentHeight: text.contentHeight,
          ...(text.inkBottom !== undefined ? { inkBottom: text.inkBottom } : {}),
          insets: { t: text.insets.t, b: text.insets.b },
          anchor: text.anchor,
        },
      ]
    }),
  )
}

/** Renderer-only refit used by native syncAutofitScale after a text edit. */
export async function measureAutofit(
  input: unknown,
  callback: NativeCallback,
  signal: AbortSignal,
) {
  input = reviveSlideNumbers(input)
  if (
    !input ||
    typeof input !== 'object' ||
    !('body' in input) ||
    !('size' in input) ||
    !('fitWidthPx' in input) ||
    !('boxWidthPx' in input) ||
    !('boxHeightPx' in input)
  ) {
    throw new TypeError('invalid slide autofit measurement input')
  }
  const request = input as {
    body: TextBody
    size: SlideDeck['size']
    fitWidthPx: number
    boxWidthPx: number
    boxHeightPx: number
  }
  await ensureSlidesEngine(signal)
  const refit = await withSlidesFontMetrics(
    (metrics) =>
      layoutText({
        body: request.body,
        boxWidthPx: request.boxWidthPx,
        boxHeightPx: request.boxHeightPx,
        metrics,
        vp: makeViewport(request.size, request.fitWidthPx),
        refitAutofit: true,
      }),
    callback,
    signal,
  )
  return encodeSlideNumbers({
    fontScale: refit.fontScale,
    lnSpcReduction: refit.lnSpcReduction ?? 0,
  })
}
