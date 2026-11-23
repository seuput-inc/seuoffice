import { defaultClientConditions, defaultServerConditions } from 'vite'
import { defineConfig } from 'vitest/config'

export default defineConfig({
  resolve: { conditions: ['seuoffice-source', ...defaultClientConditions] },
  ssr: { resolve: { conditions: ['seuoffice-source', ...defaultServerConditions] } },
  test: {
    include: ['tests/**/*.test.ts'],
    environment: 'jsdom',
    testTimeout: 20000,
  },
})
