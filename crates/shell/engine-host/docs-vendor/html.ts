import { convertHtmlToDocx } from '../../../../packages/html2docx/src/convert'
import type { BrowserDriver } from '../../../../packages/html2docx/src/driver'
import { encode } from '../../bridge/wire'

/** The driver owns one isolated conversion page and must destroy it on close. */
export async function convertHtml(
  url: string,
  driver: BrowserDriver,
  signal: AbortSignal,
  naturalTableWidth: boolean,
): Promise<unknown> {
  signal.throwIfAborted()
  let closing: Promise<void> | undefined
  const close = () => {
    closing ??= driver.close()
    return closing
  }
  const abort = () => {
    void close().catch(() => {})
  }
  signal.addEventListener('abort', abort, { once: true })
  try {
    const { docx } = await convertHtmlToDocx({ url }, driver, { naturalTableWidth, signal })
    const frame = encode(docx)
    return {
      header: frame.header,
      buffers: frame.buffers.map((bytes) => {
        let text = ''
        for (const byte of bytes) text += String.fromCharCode(byte)
        return btoa(text)
      }),
    }
  } finally {
    signal.removeEventListener('abort', abort)
    await close()
  }
}
