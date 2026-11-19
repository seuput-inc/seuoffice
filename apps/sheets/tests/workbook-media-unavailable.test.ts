/** @vitest-environment jsdom */
/**
 * A workbook picture whose bytes cannot be read leaves its own frame empty.
 * Whatever the host does with the request (answers, refuses, throws, or has
 * no reader at all, as a web bridge may), the rest of the sheet stays up.
 */
import { act, createElement } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi, type MockInstance } from 'vitest'

import { useWorkbookMediaUrl } from '../src/renderer/workbook-media'

const SESSION = '0b0f5c1e-3f0c-4a57-9d2b-6f1f1f6f8a11'
const PNG = { mediaType: 'image/png', base64: 'iVBORw0KGgo=' }

/** A sheet as the grid mounts it: its cells, and one picture beside them */
function Sheet(): React.JSX.Element {
  const { url, unavailable } = useWorkbookMediaUrl(SESSION, 'picture-1')
  const picture = unavailable ? 'unavailable' : (url ?? 'loading')
  return createElement(
    'div',
    null,
    createElement('span', { className: 'cells' }, 'cells'),
    createElement('span', { className: 'picture' }, picture),
  )
}

let host: HTMLDivElement
let root: Root
let warn: MockInstance<typeof console.warn>

beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})

afterEach(() => {
  act(() => root.unmount())
  host.remove()
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

/** mounts the sheet on a host with this desktopApi and lets the read settle */
async function mountOn(
  desktopApi: object,
): Promise<{ cells: string | null; picture: string | null }> {
  vi.stubGlobal('desktopApi', desktopApi)
  await act(async () => root.render(createElement(Sheet)))
  return {
    cells: host.querySelector('.cells')?.textContent ?? null,
    picture: host.querySelector('.picture')?.textContent ?? null,
  }
}

describe('a workbook picture', () => {
  it('shows once the host answers with its bytes', async () => {
    const shown = await mountOn({ readWorkbookMedia: async () => PNG })
    expect(shown).toEqual({ cells: 'cells', picture: `data:image/png;base64,${PNG.base64}` })
    expect(warn).not.toHaveBeenCalled()
  })

  it.each([
    ['refuses the read', { readWorkbookMedia: () => Promise.reject(new Error('no such part')) }],
    [
      'throws on the read',
      {
        readWorkbookMedia: () => {
          throw new Error('reader broke')
        },
      },
    ],
    ['has no reader', {}],
  ])('is left out when the host %s, and the cells stay', async (_, desktopApi) => {
    const shown = await mountOn(desktopApi)
    expect(shown).toEqual({ cells: 'cells', picture: 'unavailable' })
    expect(warn).toHaveBeenCalledTimes(1)
    expect(String(warn.mock.calls[0]![0])).toContain('picture-1')
  })
})
