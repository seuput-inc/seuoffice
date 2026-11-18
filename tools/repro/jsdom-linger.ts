// Throwaway: a jsdom environment that writes, per test file, how many editors
// (views of the floating picture frame plugin) were still alive at teardown,
// then stays alive so a timer they left running fires and vitest reports it.
import { appendFileSync } from 'node:fs'
import { builtinEnvironments, type Environment } from 'vitest/environments'

export default <Environment>{
  name: 'jsdom-linger',
  viteEnvironment: 'client',
  async setup(global, options) {
    const env = await builtinEnvironments.jsdom.setup(global, options)
    return {
      async teardown(g) {
        const live = (globalThis as { __liveFrameViews?: Set<unknown> }).__liveFrameViews?.size ?? 0
        const file = (globalThis as { __vitest_worker__?: { filepath?: string } }).__vitest_worker__
          ?.filepath
        if (live > 0) appendFileSync(process.env.LIVE_LOG!, `${live}\t${file}\n`)
        await env.teardown(g)
        await new Promise((r) => setTimeout(r, 400))
      },
    }
  },
}
