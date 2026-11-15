'use strict'

const fs = require('node:fs')
const path = require('node:path')
const os = require('node:os')
const { app, BrowserWindow } = require('electron')

const [url, output] = process.argv.slice(2)
let outcome
let window
const result = {
  driver: 'electron',
  status: 'failed',
  outcome: null,
  screenshot: { status: 'failed', path: 'screenshot.png' },
  pdf: { status: 'failed', path: 'document.pdf' },
}
let timer

async function main() {
  if (!url || !output || process.argv.length !== 4)
    throw new Error('Usage: electron electron.cjs URL OUTPUT_DIR')
  fs.mkdirSync(output, { recursive: true })
  const parsed = new URL(url)
  if (parsed.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(parsed.hostname))
    throw new Error('URL must use loopback HTTP')
  const userData = path.resolve(output, 'electron-user-data')
  fs.mkdirSync(userData, { recursive: true })
  app.setPath('userData', userData)
  app.setPath('sessionData', userData)
  await app.whenReady()
  window = new BrowserWindow({
    width: 1280,
    height: 900,
    useContentSize: true,
    show: true,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      partition: 'probe',
    },
  })
  await window.loadURL(url)
  for (;;) {
    const raw = await window.webContents.executeJavaScript('JSON.stringify(window.__probeOutcome)')
    if (raw) {
      outcome = JSON.parse(raw)
      result.outcome = outcome
      if (outcome.status === 'failed')
        throw new Error(`Probe failed: ${JSON.stringify(outcome.error)}`)
      if (outcome.status === 'complete') break
      if (outcome.status !== 'running')
        throw new Error(`Unexpected probe status: ${outcome.status}`)
    }
    await new Promise((resolve) => setTimeout(resolve, 250))
  }
  const viewport = await window.webContents.executeJavaScript(
    '({width: innerWidth, height: innerHeight, devicePixelRatio, userAgent: navigator.userAgent})',
  )
  if (viewport.width !== 1280 || viewport.height !== 900)
    throw new Error(`Unexpected viewport: ${JSON.stringify(viewport)}`)
  const image = await window.webContents.capturePage()
  if (image.isEmpty()) throw new Error('capturePage returned an empty image')
  const pixels = image.getSize()
  fs.writeFileSync(path.join(output, 'screenshot.png'), image.toPNG())
  result.screenshot = {
    status: 'complete',
    path: 'screenshot.png',
    viewport,
    imagePixels: pixels,
    electron: process.versions.electron,
    chromium: process.versions.chrome,
    os: os.release(),
  }
  const prepared = JSON.parse(
    await window.webContents.executeJavaScript('window.__prepareProbePrint()'),
  )
  const pdf = await window.webContents.printToPDF({
    pageSize: 'A4',
    margins: { top: 0, bottom: 0, left: 0, right: 0 },
    printBackground: true,
    preferCSSPageSize: true,
  })
  if (pdf.length < 5 || pdf.subarray(0, 5).toString() !== '%PDF-')
    throw new Error('printToPDF returned invalid PDF')
  fs.writeFileSync(path.join(output, 'document.pdf'), pdf)
  result.pdf = {
    status: 'complete',
    path: 'document.pdf',
    printMode: 'diagnostic-cloned-dom',
    printPreparation: prepared,
  }
  result.status = 'complete'
}

;(async () => {
  let exitCode = 0
  try {
    await Promise.race([
      main(),
      new Promise((_, reject) => {
        timer = setTimeout(
          () => reject(new Error('Diagnostic deadline exceeded (180 seconds)')),
          180000,
        )
      }),
    ])
  } catch (error) {
    exitCode = 1
    result.error = String((error && error.stack) || error)
    console.error(result.error)
  } finally {
    clearTimeout(timer)
    if (output) {
      try {
        fs.mkdirSync(output, { recursive: true })
        fs.writeFileSync(path.join(output, 'result.json'), JSON.stringify(result, null, 2) + '\n')
      } catch (error) {
        exitCode = 1
        console.error(`Cannot write result.json: ${error}`)
      }
    }
    if (window && !window.isDestroyed()) window.destroy()
    app.exit(exitCode)
  }
})()
