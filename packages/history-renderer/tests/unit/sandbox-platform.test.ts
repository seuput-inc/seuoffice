import { describe, expect, it } from 'vitest'
import { denoPackage } from '../../src/sandbox'

const glibc = () => '2.39'
const musl = () => undefined

describe('denoPackage', () => {
  it.each([
    ['darwin', 'arm64', '@deno/darwin-arm64'],
    ['linux', 'arm64', '@deno/linux-arm64-glibc'],
    ['linux', 'x64', '@deno/linux-x64-glibc'],
  ])('runs on %s %s', (platform, arch, name) => {
    expect(denoPackage(platform, arch, glibc)).toBe(name)
  })

  it.each([
    ['darwin', 'x64'],
    ['win32', 'x64'],
    ['win32', 'arm64'],
    ['linux', 'ia32'],
    ['freebsd', 'x64'],
  ])('refuses %s %s', (platform, arch) => {
    expect(() => denoPackage(platform, arch, glibc)).toThrow(
      expect.objectContaining({ code: 'unsupported', message: expect.stringContaining(arch) }),
    )
  })

  it.each(['arm64', 'x64'])('refuses musl Linux %s before running a glibc binary', (arch) => {
    expect(() => denoPackage('linux', arch, musl)).toThrow(
      expect.objectContaining({ code: 'unsupported', message: expect.stringContaining('musl') }),
    )
  })
})
