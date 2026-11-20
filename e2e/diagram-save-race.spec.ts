import { rmSync } from 'node:fs'
import { test, expect } from '@playwright/test'
import { closeAndSaveVideo, launchShell } from './helpers'
import {
  answerMessageBoxes,
  applyOps,
  clickMenu,
  closeTab,
  contentsIdOf,
  copyAsset,
  diagramTabs,
  fileText,
  liveText,
  messageBoxes,
  openDiagramPage,
  openPath,
  scratchDir,
  statusText,
} from './diagram-lib'

test('an edit made while save is awaiting its reply still prompts before close', async () => {
  test.setTimeout(180_000)
  const dir = await scratchDir('save-race')
  const launched = await launchShell({ onboardingSeen: true, videoDir: 'diagram-save-race' })
  const { app, page: shell } = launched
  try {
    await answerMessageBoxes(app, 2)
    const path = await copyAsset('sample.drawio', dir)
    const page = await openDiagramPage(app, () => openPath(shell, path))
    const contentsId = await contentsIdOf(app, page)
    await app.evaluate(({ ipcMain }, id) => {
      const handlers: unknown = Reflect.get(ipcMain, '_invokeHandlers')
      if (!(handlers instanceof Map)) throw new Error('Electron invoke handlers are unavailable')
      const original: unknown = handlers.get('diagram:save')
      if (typeof original !== 'function') throw new Error('diagram save handler is not registered')
      const state = {
        arrived: false,
        release: undefined as (() => void) | undefined,
        dirty: [] as boolean[],
      }
      Reflect.set(globalThis, '__diagramSaveRace', state)
      ipcMain.on('diagram:dirty-changed', (event, dirty: unknown) => {
        if (event.sender.id === id && typeof dirty === 'boolean') state.dirty.push(dirty)
      })
      ipcMain.removeHandler('diagram:save')
      ipcMain.handle('diagram:save', async (event, ...args: unknown[]) => {
        const result: unknown = await original(event, ...args)
        if (event.sender.id === id) {
          const { promise, resolve } = Promise.withResolvers<void>()
          state.release = resolve
          state.arrived = true
          await promise
        }
        return result
      })
    }, contentsId)

    const first = await applyOps(app, contentsId, [
      { op: 'add_shape', id: 'before-save', label: 'Before save', x: 420, y: 200 },
    ])
    expect(first.ok, first.error).toBe(true)
    await expect.poll(() => statusText(page)).toBe('Unsaved changes')
    await expect
      .poll(() =>
        app.evaluate(() => {
          const host = globalThis as typeof globalThis & { __diagramSaveRace: { dirty: boolean[] } }
          return host.__diagramSaveRace.dirty.at(-1)
        }),
      )
      .toBe(true)
    await clickMenu(app, 'File', '^Save$')
    await expect
      .poll(
        () =>
          app.evaluate(() => {
            const host = globalThis as typeof globalThis & {
              __diagramSaveRace: { arrived: boolean }
            }
            return host.__diagramSaveRace.arrived
          }),
        { timeout: 30_000 },
      )
      .toBe(true)
    const savedSnapshot = await fileText(path)
    expect(savedSnapshot).toContain('Before save')
    expect(savedSnapshot).not.toContain('During save')
    const dirtyCount = await app.evaluate(() => {
      const host = globalThis as typeof globalThis & { __diagramSaveRace: { dirty: boolean[] } }
      return host.__diagramSaveRace.dirty.length
    })

    const second = await applyOps(app, contentsId, [
      { op: 'add_shape', id: 'during-save', label: 'During save', x: 620, y: 200 },
    ])
    expect(second.ok, second.error).toBe(true)
    expect(await liveText(page)).toContain('During save')
    expect(
      await app.evaluate(() => {
        const host = globalThis as typeof globalThis & { __diagramSaveRace: { dirty: boolean[] } }
        return host.__diagramSaveRace.dirty.length
      }),
    ).toBe(dirtyCount)

    await app.evaluate(() => {
      const host = globalThis as typeof globalThis & { __diagramSaveRace: { release?: () => void } }
      const state = host.__diagramSaveRace
      if (!state.release) throw new Error('save reply is not waiting')
      state.release()
    })
    await expect
      .poll(() =>
        app.evaluate(() => {
          const host = globalThis as typeof globalThis & { __diagramSaveRace: { dirty: boolean[] } }
          return host.__diagramSaveRace.dirty.length
        }),
      )
      .toBeGreaterThan(dirtyCount)
    expect(
      await app.evaluate(() => {
        const host = globalThis as typeof globalThis & { __diagramSaveRace: { dirty: boolean[] } }
        return host.__diagramSaveRace.dirty.at(-1)
      }),
    ).toBe(true)
    await expect.poll(() => statusText(page)).toBe('Unsaved changes')
    const [tab] = await diagramTabs(shell)
    if (!tab) throw new Error('diagram tab disappeared')
    await closeTab(shell, tab.id)
    await expect.poll(async () => (await messageBoxes(app)).length).toBe(1)
    expect((await diagramTabs(shell)).map((entry) => entry.id)).toContain(tab.id)
    expect(await fileText(path)).toBe(savedSnapshot)
  } finally {
    await app
      .evaluate(() => {
        const host = globalThis as typeof globalThis & {
          __diagramSaveRace?: { release?: () => void }
        }
        host.__diagramSaveRace?.release?.()
      })
      .catch(() => {})
    await closeAndSaveVideo(launched, 'diagram-save-race')
    rmSync(dir, { recursive: true, force: true })
  }
})
