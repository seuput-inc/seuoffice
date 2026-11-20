import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'

const html = readFileSync(new URL('./index.html', import.meta.url), 'utf8')
const policy = /http-equiv="Content-Security-Policy"\s+content="([^"]+)"/.exec(html)?.[1] ?? ''
const connectSrc = policy
  .split(';')
  .map((part) => part.trim().split(/\s+/))
  .find(([name]) => name === 'connect-src')
  ?.slice(1)

describe('the engine page', () => {
  it('lets the shell bridge reach the shell: Tauri ipc protocol on macOS and on Windows', () => {
    // the bridge has no postMessage fallback, so a policy that blocks these fetches leaves the page
    // unable to send anything, readiness included
    expect(connectSrc).toEqual(expect.arrayContaining(["'self'", 'ipc:', 'http://ipc.localhost']))
  })

  it('runs its boot script before the module bundle', () => {
    const scripts = [...html.matchAll(/<script\b[^>]*>/g)].map((match) => match[0])
    expect(scripts).toEqual(['<script src="./boot.js">', '<script type="module" src="./main.js">'])
  })
})
