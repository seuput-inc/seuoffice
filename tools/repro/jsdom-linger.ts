// A jsdom environment whose worker stays alive after teardown, the way a busy
// CI worker does: a timer a test file left running then fires with the DOM
// globals already gone, instead of only when the machine happens to be slow.
import { builtinEnvironments, type Environment } from 'vitest/environments'

export default <Environment>{
  name: 'jsdom-linger',
  viteEnvironment: 'client',
  async setup(global, options) {
    const env = await builtinEnvironments.jsdom.setup(global, options)
    return {
      async teardown(g) {
        await env.teardown(g)
        await new Promise((r) => setTimeout(r, 400))
      },
    }
  },
}
