import type { BrowserDriver, ScreenshotOptions, Viewport } from '../../../../packages/html2docx/src/driver'

/** Each callback is scoped to one request and one dedicated CEF page. */
export type PageCall = (operation: string, input: unknown) => Promise<unknown>

export class CefHtmlDriver implements BrowserDriver {
  constructor(private readonly call: PageCall) {}

  async setViewport(viewport: Viewport): Promise<void> {
    await this.call('html.viewport', viewport)
  }

  async goto(url: string, options: { timeoutMs: number }): Promise<void> {
    await this.call('html.goto', { url, timeoutMs: options.timeoutMs })
  }

  evaluate: BrowserDriver['evaluate'] = async (fn, arg) => {
    const expression = typeof fn === 'string' ? fn : `(${fn.toString()})(${arg === undefined ? '' : JSON.stringify(arg)})`
    return await this.call('html.evaluate', { expression }) as never
  }

  async addScript(source: string): Promise<void> {
    await this.call('html.evaluate', { expression: source })
  }

  async addInitScript(source: string): Promise<void> {
    await this.call('html.initScript', { source })
  }

  async screenshot(options: ScreenshotOptions): Promise<Uint8Array> {
    const result = await this.call('html.screenshot', options)
    if (typeof result !== 'string') throw new Error('HTML screenshot returned invalid data')
    return Uint8Array.from(atob(result), character => character.charCodeAt(0))
  }

  async screenshotElement(selector: string, options: Pick<ScreenshotOptions, 'omitBackground'>): Promise<Uint8Array | null> {
    const box = await this.evaluate((sel: string) => {
      const node = document.querySelector(sel)
      if (!node) return null
      const rect = node.getBoundingClientRect()
      return {
        x: Math.max(0, Math.round(rect.left + window.scrollX)),
        y: Math.max(0, Math.round(rect.top + window.scrollY)),
        width: Math.round(rect.width),
        height: Math.round(rect.height),
      }
    }, selector)
    if (!box || box.width <= 0 || box.height <= 0) return null
    return this.screenshot({ clip: box, omitBackground: options.omitBackground })
  }

  async elementExists(selector: string): Promise<boolean> {
    return this.evaluate((sel: string) => document.querySelector(sel) !== null, selector)
  }

  async close(): Promise<void> {
    await this.call('html.close', null)
  }
}
