import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { runInNewContext } from 'node:vm'

const shell = resolve(import.meta.dirname, '../../apps/shell')
const require = createRequire(join(shell, 'electron-builder.cjs'))

function releaseConfig(platform, env, signAsync, seam) {
  function load(file) {
    const module = { exports: {} }
    runInNewContext(readFileSync(join(shell, file), 'utf8'), {
      module,
      __dirname: shell,
      process: { platform, env, argv: seam?.argv ?? ['--publish', 'never'] },
      require(id) {
        if (id === './electron-builder.cjs' || id === './electron-builder.seuoffice.cjs') {
          return load(id)
        }
        if (seam && id === './package.json') return { version: '1.2.3' }
        if (seam && id === 'node:fs') {
          const fs = require(id)
          return {
            ...fs,
            readFileSync: (path, ...args) =>
              path === env.GITHUB_EVENT_PATH
                ? JSON.stringify(seam.event)
                : fs.readFileSync(path, ...args),
          }
        }
        if (id === 'electron-builder') return { Arch: {} }
        if (id === 'electron/package.json') return { version: '42.0.0' }
        if (id === '@electron/osx-sign') return { signAsync }
        if (id === 'node:child_process') return {}
        return require(id)
      },
    })
    return module.exports
  }
  return load(seam ? 'electron-builder.migration-seam.cjs' : 'electron-builder.seuoffice.cjs')
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

const dispatchEnv = {
  GITHUB_EVENT_NAME: 'workflow_dispatch',
  GITHUB_REPOSITORY: 'seuput-inc/seuoffice',
  GITHUB_REF: 'refs/heads/integration/tauri-first-candidate',
  GITHUB_EVENT_PATH: 'event.json',
}
const inputs = { migration_seam: 'true', release: 'false', bridge_version: '1.2.3' }

for (const platform of ['darwin', 'win32']) {
  for (const channel of ['latest', 'stable']) {
    test(`${platform} ${channel} embeds the fixed feed only in the explicit QA package`, () => {
      const env = { ...dispatchEnv, SEUOFFICE_UPDATE_CHANNEL: channel }
      const config = releaseConfig(platform, env, undefined, { event: { inputs } })
      assert.deepEqual(JSON.parse(JSON.stringify(config.publish)), [
        { provider: 'generic', url: 'http://127.0.0.1:8765/', channel },
      ])
      assert.equal(config.extraMetadata.seuofficeBuildProfile, 'migration-seam')
      const suffix = `${channel === 'stable' ? '-stable' : ''}-migration-seam.\${ext}`
      assert.equal(config.nsis.artifactName, `\${productName}-Setup-\${version}${suffix}`)
      assert.equal(config.dmg.artifactName, `\${productName}-\${version}-\${arch}${suffix}`)
      assert.equal(config.mac.artifactName, `\${productName}-\${version}-\${arch}-mac${suffix}`)

      const release = releaseConfig(platform, env)
      assert.equal(release.extraMetadata.seuofficeBuildProfile, undefined)
      assert.deepEqual(JSON.parse(JSON.stringify(release.publish)), [
        { provider: 'github', owner: 'seuput-inc', repo: 'seuoffice-tauri-releases', channel },
      ])
    })
  }
}

test('QA packaging rejects publishing, stale version stamps and non-bridge dispatches', () => {
  for (const change of [
    { release: 'true' },
    { migration_seam: 'false' },
    { bridge_version: '1.2.2' },
    { candidate_version: '1.2.3' },
    { next_version: '1.2.4' },
  ]) {
    const event = { inputs: { ...inputs, ...change } }
    assert.throws(
      () => releaseConfig('win32', dispatchEnv, undefined, { event }),
      /explicit bridge dispatch/,
    )
  }
  for (const change of [
    { GITHUB_REF: 'refs/heads/main' },
    { GITHUB_REF: 'refs/tags/v1.2.3' },
    { GITHUB_EVENT_NAME: 'push' },
    { GITHUB_REPOSITORY: 'fork/seuoffice' },
  ]) {
    const env = { ...dispatchEnv, ...change }
    assert.throws(
      () => releaseConfig('darwin', env, undefined, { event: { inputs } }),
      /nonpublishing candidate dispatch/,
    )
  }
  for (const argv of [[], ['--publish', 'always']]) {
    assert.throws(
      () => releaseConfig('win32', dispatchEnv, undefined, { event: { inputs }, argv }),
      /nonpublishing candidate dispatch/,
    )
  }
})
