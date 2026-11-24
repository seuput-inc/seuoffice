import { defineConfig } from 'vitest/config'

export default defineConfig({
  resolve: { conditions: ['seuoffice-source', 'import', 'default'] },
  // Node-environment suites resolve through the SSR conditions, which need the source condition too.
  ssr: { resolve: { conditions: ['seuoffice-source', 'node', 'import', 'default'] } },
  test: { include: ['tests/**/*.test.ts'], environment: 'node' },
})
