import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { runInNewContext } from 'node:vm'

const shell = resolve(import.meta.dirname, '../../apps/shell')
const require = createRequire(join(shell, 'electron-builder.cjs'))

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

for (const channel of ['latest', 'stable']) {
  test(`the ${channel} Mac bridge signs ad hoc despite a discovered identity`, async (t) => {
    const sign = t.mock.fn()
    const config = releaseConfig('darwin', { SEUOFFICE_UPDATE_CHANNEL: channel }, sign)
    assert.equal(config.mac.identity, '-')
    assert.equal(config.mac.hardenedRuntime, false)
    assert.equal(config.mac.notarize, false)
    assert.equal(config.dmg.sign, false)
    assert.equal(config.afterAllArtifactBuild, null)

    const optionsForFile = () => ({ entitlements: 'build/entitlements.mac.plist' })
    const app = 'release/mac-arm64/SeuOffice.app'
    await config.mac.sign({ app, identity: 'Developer ID Application: Test-Only', optionsForFile })
    assert.equal(sign.mock.callCount(), 1)
    const [options] = sign.mock.calls[0].arguments
    assert.equal(options.app, app)
    assert.equal(options.optionsForFile, optionsForFile)
    assert.equal(options.identity, '-')
    assert.equal(options.identityValidation, false)
  })
}

test('Mac packaging refuses any CSC_LINK before builder can import a certificate', () => {
  for (const link of ['', 'other.p12']) {
    assert.throws(() => releaseConfig('darwin', { CSC_LINK: link }), /CSC_LINK/)
  }
})

test('a signing failure stops packaging instead of emitting an unsigned app', async () => {
  const config = releaseConfig('darwin', {}, async () => {
    throw new Error('codesign failed')
  })
  await assert.rejects(config.mac.sign({ app: 'SeuOffice.app' }), /codesign failed/)
})

test('Mac signing does not override Windows signing', () => {
  const config = releaseConfig('win32', { SEUOFFICE_WIN_SIGN_MODE: 'production' })
  assert.equal(typeof config.win.signtoolOptions.sign, 'function')
  assert.equal(config.mac.sign, undefined)
})
