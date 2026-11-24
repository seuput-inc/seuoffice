import { defaultClientConditions, defaultServerConditions } from 'vite'
import { defineConfig } from 'vitest/config'
import {
  ENGINE_NODE_SETUP,
  engineWasmAsset,
  engineWasmEnv,
} from '../../packages/engine-wasm/src/vitest'

export default defineConfig({
  // the engine behind @seuoffice/file-parse: the mains start it from a ?asset path
  plugins: [engineWasmAsset()],
  // seuoffice: the web bridge imports @seuoffice/web-platform, whose co-editing page reads
  // @seuoffice/collab, which ships TypeScript source under this export condition
  resolve: { conditions: ['seuoffice-source', ...defaultClientConditions] },
  // Node-environment suites resolve through the SSR conditions, which need the source condition too.
  ssr: { resolve: { conditions: ['seuoffice-source', ...defaultServerConditions] } },
  test: {
    include: ['tests/**/*.test.ts'],
    env: engineWasmEnv(),
    setupFiles: [ENGINE_NODE_SETUP],
    environment: 'node',
    testTimeout: 20000,
  },
})
