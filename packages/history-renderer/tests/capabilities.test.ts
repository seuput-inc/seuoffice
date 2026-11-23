import { randomUUID } from 'node:crypto'
import { once } from 'node:events'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { expect, it } from 'vitest'

it('refuses host credentials, a readable file, a live loopback listener and process execution', async () => {
  // Exercise the installed launcher's policy without publishing a caller-supplied guest path.
  const { launch } = await import(
    new URL('sandbox.js', import.meta.resolve('@seuoffice/history-renderer')).href
  )
  const dir = await mkdtemp(join(tmpdir(), 'history-probe-'))
  const path = join(dir, 'canary')
  const marker = randomUUID()
  const previous = process.env.HISTORY_TEST_CREDENTIAL
  const listener = createServer((socket) => socket.end(marker))
  try {
    await writeFile(path, marker)
    process.env.HISTORY_TEST_CREDENTIAL = marker
    listener.listen(0, '127.0.0.1')
    await once(listener, 'listening')
    const address = listener.address()
    if (!address || typeof address === 'string') throw new Error('no test listener')
    const result = await launch(
      new URL('./capabilities.mjs', import.meta.url),
      Buffer.from(JSON.stringify({ path, port: address.port })),
    )
    expect(JSON.parse(result.toString())).toEqual({
      credential: 'NotCapable',
      read: 'NotCapable',
      write: 'NotCapable',
      socket: 'NotCapable',
      process: 'NotCapable',
      ffi: 'denied',
      sys: 'denied',
      import: 'denied',
    })
    expect(await readFile(path, 'utf8')).toBe(marker)
  } finally {
    listener.close()
    if (previous === undefined) delete process.env.HISTORY_TEST_CREDENTIAL
    else process.env.HISTORY_TEST_CREDENTIAL = previous
    await rm(dir, { force: true, recursive: true })
  }
})
