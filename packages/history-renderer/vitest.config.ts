import { defineConfig } from 'vitest/config'

export default defineConfig({
  resolve: { conditions: ['seuoffice-source', 'import', 'default'] },
  // Node-environment suites resolve through the SSR conditions, which need the source condition too.
  ssr: { resolve: { conditions: ['seuoffice-source', 'node', 'import', 'default'] } },
  // Every render starts a Deno sandbox; a test of a dozen renders outlasts 5 s beside the corpus run.
  test: { include: ['tests/**/*.test.ts'], environment: 'node', testTimeout: 30_000 },
})
