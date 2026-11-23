import { defineConfig } from 'vitest/config'

export default defineConfig({
  resolve: { conditions: ['seuoffice-source', 'import', 'default'] },
  test: { include: ['tests/**/*.test.ts'], environment: 'node' },
})
