import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { runInNewContext } from 'node:vm'

const shell = resolve(import.meta.dirname, '../../apps/shell')
const require = createRequire(join(shell, 'electron-builder.cjs'))
const fingerprint = '0123456789ABCDEF'.repeat(2) + '01234567'
const candidate = { SEUOFFICE_BRIDGE_CANDIDATE: 'true', SEUOFFICE_MAC_SIGN_IDENTITY: fingerprint }

function releaseConfig(platform, env, signAsync) {
  function load(file) {
    const module = { exports: {} }
    runInNewContext(readFileSync(join(shell, file), 'utf8'), {
      module,
      __dirname: shell,
      process: { platform, env },
      require(id) {
        if (id === './electron-builder.cjs') return load(id)
        if (id === 'electron-builder') return { Arch: {} }
        if (id === 'electron/package.json') return { version: '42.0.0' }
        if (id === '@electron/osx-sign') return { signAsync }
        if (id === 'node:child_process') return {}
        return require(id)
      },
    })
    return module.exports
  }
  return load('electron-builder.seuoffice.cjs')
}

test('the Mac bridge signs the packager bundle with the imported fingerprint, without notarization', async (t) => {
  const sign = t.mock.fn()
  const config = releaseConfig('darwin', candidate, sign)
  assert.equal(config.mac.identity, '-') // The builder gate that invokes mac.sign.
  assert.equal(config.mac.notarize, false)
  assert.equal(config.mac.timestamp, 'none')
  assert.equal(config.dmg.sign, false)
  assert.equal(config.afterAllArtifactBuild, null)

  const optionsForFile = () => ({ entitlements: 'build/entitlements.mac.plist' })
  const app = 'release/mac-arm64/SeuOffice.app'
  await config.mac.sign({ app, identity: '-', optionsForFile })
  assert.equal(sign.mock.callCount(), 1)
  const [options] = sign.mock.calls[0].arguments
  assert.equal(options.app, app)
  assert.equal(options.optionsForFile, optionsForFile)
  assert.equal(options.identity, fingerprint)
  assert.equal(options.identityValidation, false)
})

test('a bridge cannot fall back to ad hoc or Developer ID if the fixed signer is unavailable', async () => {
  for (const identity of [undefined, '-', 'SeuOffice Self Sign', '0'.repeat(39), 'Z'.repeat(40)]) {
    assert.throws(
      () => releaseConfig('darwin', { ...candidate, SEUOFFICE_MAC_SIGN_IDENTITY: identity }),
      /SEUOFFICE_MAC_SIGN_IDENTITY/,
    )
  }
  assert.throws(() => releaseConfig('darwin', { ...candidate, CSC_LINK: 'other.p12' }), /CSC_LINK/)
  const config = releaseConfig('darwin', candidate, async () => {
    throw new Error('identity unavailable')
  })
  await assert.rejects(config.mac.sign({ identity: '-' }), /identity unavailable/)
})

test('other platforms and ordinary Mac builds do not select the bridge signer', () => {
  for (const config of [
    releaseConfig('win32', candidate),
    releaseConfig('darwin', { ...candidate, SEUOFFICE_BRIDGE_CANDIDATE: 'false' }),
  ]) {
    assert.equal(config.mac.sign, undefined)
    assert.equal(config.mac.identity, '-')
    assert.equal(config.afterAllArtifactBuild, 'build/notarize-dmg.js')
  }
})
