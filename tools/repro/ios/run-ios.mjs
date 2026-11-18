/**
 * `node tools/repro/ios/run-ios.mjs <site> <out>`: opens every case in Mobile Safari on an iPhone
 * simulator and keeps what the pages measured, with a screenshot of each as shown and one per
 * variant the page tries in place (probe.js). REPRO_VARIANTS=none keeps to the page as shown.
 *
 * IOS_VERSION picks a runtime by its version prefix (default: the newest installed).
 */
import { execFileSync } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { setTimeout as sleep } from 'node:timers/promises'
import { CASES, start } from './serve.mjs'
import { summarize } from './summarize.mjs'

const site = resolve(process.argv[2] ?? 'ios-repro-site')
const out = resolve(process.argv[3] ?? 'ios-repro-out')
mkdirSync(out, { recursive: true })

const simctl = (...args) =>
  execFileSync('xcrun', ['simctl', ...args], { encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 })

// 390 points wide first: the width the report came from
const PHONES = ['iPhone 17e', 'iPhone 16e', 'iPhone 14', 'iPhone 13', 'iPhone 12', 'iPhone 17', 'iPhone 16', 'iPhone 15']

function newPhone() {
  const version = (text) => text.split('.').map(Number)
  const newer = (a, b) => {
    const [x, y] = [version(a.version), version(b.version)]
    for (let i = 0; i < 3; i++) if ((x[i] ?? 0) !== (y[i] ?? 0)) return (y[i] ?? 0) - (x[i] ?? 0)
    return 0
  }
  const wanted = (runtime) => {
    const at = PHONES.findIndex((name) => runtime.supportedDeviceTypes.some((t) => t.name === name))
    return at < 0 ? PHONES.length : at
  }
  const runtimes = JSON.parse(simctl('list', '-j', 'runtimes'))
    .runtimes.filter((r) => r.isAvailable && r.platform === 'iOS')
    .filter((r) => r.version.startsWith(process.env.IOS_VERSION ?? ''))
    // a version can come as several builds, one of them for a single new device
    .sort((a, b) => newer(a, b) || wanted(a) - wanted(b))
  if (!runtimes.length) throw new Error('no iOS simulator runtime')
  const runtime = runtimes[0]
  const types = runtime.supportedDeviceTypes
  const type =
    PHONES.map((name) => types.find((t) => t.name === name)).find(Boolean) ??
    types.find((t) => t.name.startsWith('iPhone'))
  if (!type) throw new Error(`no iPhone for ${runtime.name}`)
  const udid = simctl('create', 'seuoffice-repro', type.identifier, runtime.identifier).trim()
  return { udid, name: `${type.name}, ${runtime.name} (${runtime.buildversion})` }
}

const phone = newPhone()
console.log(`simulator: ${phone.name}`)
simctl('boot', phone.udid)
simctl('bootstatus', phone.udid, '-b')

const server = await start({ site })

async function open(url) {
  // Safari is not there the moment a new simulator reports it has booted
  for (let attempt = 1; ; attempt++) {
    try {
      simctl('openurl', phone.udid, url)
      return
    } catch (error) {
      if (attempt === 6) throw error
      await sleep(10_000)
    }
  }
}
const shot = (name) => simctl('io', phone.udid, 'screenshot', resolve(out, name))

try {
  await open(`${server.hostUrl}/warm.html`)
  await sleep(20_000)
  shot('00-warm.png')

  for (const test of CASES) {
    console.log(`\n== ${test.name}`)
    server.begin(test.name)
    await open(server.pageUrl(test))
    let shown = await server.waitFor(test.name, 'as-shown', 75_000)
    if (!shown) {
      // the host never heard `opened`: whatever the frames show is still worth its numbers
      server.setPhase('as-shown')
      shown = await server.waitFor(test.name, 'as-shown', 15_000)
    }
    shot(`${test.name}.png`)
    if (!shown) {
      console.log(`${test.name}: nothing measured`)
      continue
    }
    for (const variant of process.env.REPRO_VARIANTS === 'none' ? [] : test.variants) {
      server.setPhase(variant)
      await server.waitFor(test.name, variant, 30_000)
      await sleep(500)
      shot(`${test.name}.${variant}.png`)
    }
  }
} finally {
  writeFileSync(resolve(out, 'reports.json'), JSON.stringify(server.reports, null, 1))
  const summary = summarize(server.reports, CASES, phone.name)
  writeFileSync(resolve(out, 'summary.md'), summary)
  console.log(`\n${summary}`)
}
await server.close()
simctl('shutdown', phone.udid)
