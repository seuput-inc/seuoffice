import { test } from '@playwright/test'
import { HistoryError } from '../../../packages/collab/src/client/index'
import * as historyServer from '../../../packages/collab/src/server/index'
import { examples, verifyHistory } from './collab-history-cases'
import {
  BUNDLE_HOST,
  buildEmbedScript,
  HOST_NAME,
  type HostWindow,
  origin,
  serveHostAndBundle,
} from './cross-origin-embed'

test.beforeAll(buildEmbedScript)
for (const example of examples) {
  test(`${example.name} history reopens at a version and compensates only an independent edit`, async ({
    page,
  }) => {
    test.setTimeout(180_000)
    const { host, bundle } = await serveHostAndBundle()
    try {
      await page.goto(`${origin(host, HOST_NAME)}/host.html`)
      await verifyHistory(historyServer, example, async (bytes, history) => {
        const result = await page.evaluate(
          async ({ baseUrl, fileName, bytes, history }) => {
            try {
              const sdk = (window as unknown as HostWindow).SeuofficeEmbed
              const {
                fileName: name,
                bytes: file,
                compensation,
              } = await sdk.renderHistory(document.getElementById('viewer')!, {
                baseUrl,
                fileName,
                bytes: new Uint8Array(bytes),
                history: JSON.parse(history),
              })
              return { ok: true, fileName: name, bytes: [...file], compensation } as const
            } catch (error) {
              if (!(error instanceof Error) || !('code' in error)) throw error
              return { ok: false, code: error.code } as const
            }
          },
          {
            baseUrl: `${origin(bundle, BUNDLE_HOST)}/`,
            fileName: `example.${example.name}`,
            bytes: [...bytes],
            history: JSON.stringify(history),
          },
        )
        // The page's renderer threw a HistoryError; carry its code to the server as one.
        if (!result.ok) throw new HistoryError(result.code as HistoryError['code'])
        return {
          fileName: result.fileName,
          bytes: new Uint8Array(result.bytes),
          compensation: result.compensation,
        }
      })
    } finally {
      await Promise.all([host.close(), bundle.close()])
    }
  })
}
