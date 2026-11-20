// The tests of this folder's checkers (`cd crates/shell/ci && npx vitest run`). Without this file
// vitest climbs to the root config, whose projects are the workspaces.
import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: { include: ['*.test.mjs'], testTimeout: 60000 },
})
