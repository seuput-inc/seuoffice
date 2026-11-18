// Temporary diagnostics for the macOS "quit hangs with a Word tab open" defect.
// Active only with SEUOFFICE_QUIT_TRACE=1; lines go to stdout and, when
// SEUOFFICE_QUIT_TRACE_FILE is set, to that file.
import { appendFileSync } from 'node:fs'
import { BrowserWindow, app, dialog, webContents } from 'electron'

const enabled = process.env.SEUOFFICE_QUIT_TRACE === '1'
const file = process.env.SEUOFFICE_QUIT_TRACE_FILE
const t0 = Date.now()

function log(message: string): void {
  const line = `[quit-trace +${Date.now() - t0}ms] ${message}`
  console.log(line)
  if (file) {
    try {
      appendFileSync(file, `${line}\n`)
    } catch {
      // the trace must never change behaviour
    }
  }
}

function snapshot(): string {
  const windows = BrowserWindow.getAllWindows().map(
    (w) => `${w.id}${w.isDestroyed() ? '(destroyed)' : w.isVisible() ? '(visible)' : '(hidden)'}`,
  )
  const contents = webContents
    .getAllWebContents()
    .map((c) => `${c.id}:${c.getType()}:${c.isDestroyed() ? 'destroyed' : c.getURL().slice(0, 60)}`)
  return `windows=[${windows.join(' ')}] webContents=[${contents.join(' ')}]`
}

function shortStack(): string {
  return (new Error().stack ?? '').split('\n').slice(2, 6).map((s) => s.trim()).join(' <- ')
}

if (enabled) {
  log(`enabled pid=${process.pid}`)
  const quit = app.quit.bind(app)
  const exit = app.exit.bind(app)
  const anyApp = app as unknown as Record<string, unknown>
  anyApp.quit = (): void => {
    log(`app.quit() called: ${shortStack()}`)
    quit()
  }
  anyApp.exit = (code?: number): void => {
    log(`app.exit(${code}) called: ${shortStack()}`)
    exit(code)
  }
  app.on('before-quit', (event) => {
    log(`before-quit prevented=${event.defaultPrevented} ${snapshot()}`)
  })
  const showMessageBox = dialog.showMessageBox.bind(dialog) as (...args: unknown[]) => Promise<unknown>
  ;(dialog as unknown as Record<string, unknown>).showMessageBox = (...args: unknown[]) => {
    const options = (args.find((a) => a && typeof a === 'object' && 'message' in (a as object)) ?? {}) as {
      message?: string
    }
    log(`dialog showMessageBox message=${String(options.message).slice(0, 80)}`)
    return showMessageBox(...args)
  }
  const selfQuit = Number(process.env.SEUOFFICE_QUIT_TRACE_SELF_QUIT_MS ?? 0)
  if (selfQuit > 0) setTimeout(() => app.quit(), Math.max(0, selfQuit - (Date.now() - t0)))
  app.on('will-quit', (event) => {
    log(`will-quit prevented=${event.defaultPrevented} ${snapshot()}`)
  })
  app.on('quit', (_event, exitCode) => log(`quit exitCode=${exitCode}`))
  app.on('window-all-closed', () => log(`window-all-closed ${snapshot()}`))
  process.on('exit', (code) => log(`process exit code=${code}`))
  app.on('browser-window-created', (_event, win) => {
    const id = win.id
    log(`window ${id} created`)
    win.on('close', (event) => {
      // read after the other close listeners (the shell close guard) have run
      queueMicrotask(() => log(`window ${id} close prevented=${event.defaultPrevented}`))
    })
    win.on('closed', () => log(`window ${id} closed ${snapshot()}`))
  })
  app.on('web-contents-created', (_event, contents) => {
    const id = contents.id
    contents.once('destroyed', () => log(`webContents ${id} destroyed`))
    contents.on('render-process-gone', (_e, details) =>
      log(`webContents ${id} render-process-gone reason=${details.reason}`),
    )
  })
}
