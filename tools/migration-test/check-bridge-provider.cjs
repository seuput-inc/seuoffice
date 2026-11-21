// Run on the packaging host before attesting the installers. Read, never patch, signed resources.
const assert = require('node:assert/strict')
const { createHash } = require('node:crypto')
const { createReadStream, readFileSync } = require('node:fs')
const { dirname, join } = require('node:path')
const { extractFile } = require('@electron/asar')
const { load } = require('js-yaml')

const [resources, channel, profile, manifest] = process.argv.slice(2)
assert.ok(['latest', 'stable'].includes(channel))
assert.ok(['release', 'migration-seam'].includes(profile))
const feed = load(readFileSync(join(resources, 'app-update.yml'), 'utf8'))
const pkg = JSON.parse(extractFile(join(resources, 'app.asar'), 'package.json').toString())
if (profile === 'migration-seam') {
  assert.equal(pkg.seuofficeBuildProfile, profile)
  assert.deepEqual(feed, {
    provider: 'generic',
    url: 'http://127.0.0.1:8765/',
    channel,
    updaterCacheDirName: 'seuoffice-updater',
  })
  checkSeamManifest().catch((error) => {
    console.error(error.message)
    process.exitCode = 1
  })
} else {
  assert.equal(pkg.seuofficeBuildProfile, undefined)
  assert.equal(feed.provider, 'github')
  assert.equal(feed.owner, 'seuput-inc')
  assert.equal(feed.repo, 'seuoffice-tauri-releases')
  assert.equal(feed.channel, channel)
  assert.equal(feed.url, undefined)
  assert.equal(feed.host, undefined)
}

async function checkSeamManifest() {
  const { createClient } = require('electron-updater/out/providerFactory')
  const mac = process.platform === 'darwin'
  const manifestName = `${channel}${mac ? '-mac' : ''}.yml`
  // Exercise the actual provider on the generated manifest, without a network transport.
  const client = createClient(
    feed,
    { channel: channel === 'stable' ? 'stable' : null },
    {
      platform: process.platform,
      executor: {
        request({ protocol, hostname, port, path }) {
          assert.equal(`${protocol}//${hostname}:${port}${path}`, `${feed.url}${manifestName}`)
          return Promise.resolve(readFileSync(manifest, 'utf8'))
        },
      },
    },
  )
  const info = await client.getLatestVersion()
  const suffix = `${channel === 'stable' ? '-stable' : ''}-migration-seam`
  const update = mac
    ? `SeuOffice-${pkg.version}-arm64-mac${suffix}.zip`
    : `SeuOffice-Setup-${pkg.version}${suffix}.exe`
  assert.equal(info.version, pkg.version)
  assert.equal(info.path, update)
  assert.equal(info.packages, undefined)
  const allowed = new Set([update])
  if (mac) allowed.add(`SeuOffice-${pkg.version}-arm64${suffix}.dmg`)
  for (const file of client.resolveFiles(info)) {
    const name = file.info.url
    assert.ok(allowed.delete(name), `unexpected or duplicate update file: ${name}`)
    assert.equal(file.url.href, feed.url + name)
    const hash = createHash('sha512')
    let bytes = 0
    for await (const chunk of createReadStream(join(dirname(manifest), name))) {
      bytes += chunk.length
      hash.update(chunk)
    }
    assert.equal(file.info.sha512, hash.digest('base64'))
    if (file.info.size !== undefined) assert.equal(file.info.size, bytes)
    if (name === update) assert.equal(info.sha512, file.info.sha512)
  }
  assert.ok(!allowed.has(update), 'manifest lacks its update payload')
}
