import assert from 'node:assert/strict'
import { once } from 'node:events'
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { suffixFeed } from './g6-feed.mjs'
import { initialReport, recordArrival, targets } from './g6-run.mjs'

const sha = '1'.repeat(40)
const pins = {
  G6_CHANNEL: 'stable',
  G6_BRIDGE: `1:0.12.52:${sha}`,
  TO_RUN: '2',
  TO_VERSION: '0.13.9',
  TO_SOURCE_SHA: sha,
  NEXT_RUN: '2',
  NEXT_VERSION: '0.13.10',
  NEXT_SOURCE_SHA: sha,
}
test('one attested run can supply two different ordered versions, not the same build twice', () => {
  assert.equal(targets(pins)[2][1], '0.13.10')
  for (const change of [
    { NEXT_VERSION: '0.13.9' },
    { TO_VERSION: '0.12.51' },
    { G6_BRIDGE: '1:0.12.52:main' },
    { NEXT_RUN: '' },
  ]) {
    assert.throws(() => targets({ ...pins, ...change }))
  }
})

function feeds(t) {
  const dir = mkdtempSync(join(tmpdir(), 'g6-feed-'))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  const plan = {}
  for (const [i, stage] of ['first', 'next'].entries()) {
    const version = `1.0.${i}`,
      name = `example-${version}-stable.zip`
    const sub = join(dir, stage),
      installer = join(sub, name),
      yml = join(sub, 'stable-mac.yml')
    mkdirSync(sub)
    writeFileSync(installer, `payload-${stage}`)
    writeFileSync(
      yml,
      `version: ${version}\nfiles:\n  - url: ${name}\n    sha512: c2hh\n    size: 12\npath: ${name}\nsha512: c2hh\n`,
    )
    plan[stage] = {
      version,
      installer,
      files: [
        { kind: 'archive', path: installer },
        { kind: 'feed', path: yml },
      ],
    }
  }
  return plan
}

test('loopback offers only the staged channel and a partial payload is not an installation', async (t) => {
  let stage = 'first'
  const plan = feeds(t),
    { server, observed } = suffixFeed(plan, () => stage)
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  t.after(() => {
    server.closeAllConnections()
    server.close()
  })
  const get = (p, init) => fetch(`http://127.0.0.1:${server.address().port}/${p}`, init)
  assert.equal((await get('latest-mac.yml')).status, 404)
  assert.equal((await get('example-1.0.1-stable.zip')).status, 404)
  assert.match(await (await get('stable-mac.yml')).text(), /1\.0\.0/)
  await get('example-1.0.0-stable.zip', { method: 'HEAD' })
  assert.equal(observed('first'), false)
  const range = await get('example-1.0.0-stable.zip', { headers: { range: 'bytes=2-4' } })
  assert.equal(range.status, 206)
  assert.equal(await range.text(), 'ylo')
  assert.equal(observed('first'), true)
  const result = initialReport('darwin', 'stable')
  result.hops[1].status = 'RUNNING'
  const arrival = {
    stage: 'first',
    version: '1.0.0',
    recent: true,
    document: 'a'.repeat(64),
    profile: '/guest/profile',
  }
  assert.throws(() => recordArrival(result, plan, { ...arrival, recent: false }, observed))
  recordArrival(result, plan, arrival, observed)
  assert.throws(() => recordArrival(result, plan, arrival, observed))
  stage = 'next'
  assert.equal((await get('example-1.0.0-stable.zip')).status, 404)
  assert.match(await (await get('stable-mac.yml')).text(), /1\.0\.1/)
  const arrived = { ...arrival, stage, version: '1.0.1' }
  assert.throws(() => recordArrival(result, plan, arrived, observed))
  assert.equal(await (await get('example-1.0.1-stable.zip')).text(), 'payload-next')
  for (const override of [{ profile: '/another' }, { document: 'b'.repeat(64) }]) {
    assert.throws(() => recordArrival(result, plan, { ...arrived, ...override }, observed))
  }
  recordArrival(result, plan, arrived, observed)
  assert.deepEqual(
    result.hops.map((h) => h.status),
    ['BLOCKED', 'PASSED', 'PASSED'],
  )
  assert.equal(result.g6, 'BLOCKED')
  assert.equal(result.promotable, false)
})

test('manifests cannot escape the staged files or override a download with an external URL', (t) => {
  const plan = feeds(t)
  writeFileSync(
    plan.first.files[1].path,
    'version: 1.0.0\nfiles:\n  - url: https://example.invalid/file.exe\npath: file.exe\n',
  )
  assert.throws(() => suffixFeed(plan, () => 'first'), /nonlocal/)
})
