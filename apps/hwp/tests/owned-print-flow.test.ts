// @vitest-environment node
import { describe, expect, it, vi } from 'vitest'
import { HoldTracker, runPrintJob } from '../src/renderer/print-flow'
import type { PrintJobPorts } from '../src/renderer/print-flow'

function ports(): PrintJobPorts {
  return {
    ready: true,
    hold: new HoldTracker(),
    printing: { current: false },
    bridge: {
      printPages: async () => ({
        from: 0,
        pageCount: 1,
        pages: [{ svg: '<svg/>', width: 100, height: 200 }],
      }),
    },
    api: { printPages: vi.fn(async () => ({ ok: true })) },
  }
}

describe('an owned Save As PDF renderer request', () => {
  it('echoes the same save correlation with the chosen path after gathering pages', async () => {
    const port = ports()
    await expect(
      runPrintJob(port, 'pdf', () => {}, '/chosen/copy.pdf', 'save-17'),
    ).resolves.toEqual({ ok: true })
    expect(port.api.printPages).toHaveBeenCalledWith(
      { mode: 'pdf', pages: [{ svg: '<svg/>', width: 100, height: 200 }], images: [] },
      { path: '/chosen/copy.pdf', requestToken: 'save-17' },
    )
    expect(port.printing.current).toBe(false)
  })

  it('does not turn the correlation into authority to dispatch a document that is not ready', async () => {
    const port = ports()
    port.ready = false
    await expect(
      runPrintJob(port, 'pdf', () => {}, '/chosen/copy.pdf', 'save-17'),
    ).resolves.toEqual({ ok: false, error: 'the editor is not ready' })
    expect(port.api.printPages).not.toHaveBeenCalled()
    expect(port.printing.current).toBe(false)
  })
})
