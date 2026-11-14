import { strict as assert } from 'node:assert'
import { execFileSync } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { test } from 'node:test'
import { createMutationEvidence, runComparison } from './mutation-evidence.mjs'

const scratch = process.env.OMPDESK_SCRATCH_DIR ?? tmpdir()
const root = resolve(import.meta.dirname, '../..')
const head = () =>
  execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim()
const read = (path) => JSON.parse(readFileSync(path, 'utf8'))

function fixture(t) {
  const dir = mkdtempSync(join(scratch, 'mutation-evidence-test-'))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  const directory = join(dir, 'evidence')
  const source = join(dir, 'source.rs')
  writeFileSync(source, 'before')
  const env = {
    PRODUCT_SHA: head(),
    DEFINITION_SHA: 'a'.repeat(40),
    GITHUB_RUN_ID: '123',
    GITHUB_RUN_ATTEMPT: '1',
    MAIN_DIFF_PROFILE: 'node-faults',
    CARGO_INCREMENTAL: '1',
  }
  const options = { directory, root, files: ['catalog.json'], shard: 0, shards: 1, env }
  const sink = createMutationEvidence(options)
  const fault = { name: 'example', file: 'source.rs', find: 'before', replace: 'after', fn: ['f'] }
  const faults = [fault, { ...fault, name: 'second' }]
  const catalogBytes = Buffer.from(JSON.stringify(faults))
  const catalog = sink.catalog(0, 'catalog.json', catalogBytes, faults.length)
  return { dir, directory, source, options, sink, fault, faults, catalog, catalogBytes }
}

function mutation(f, index, report = '{"ok":true}') {
  const state = f.sink.begin(f.catalog, index, f.faults[index], f.source, 'before', true, 1)
  writeFileSync(f.source, 'after')
  f.sink.applied(state)
  const work = f.sink.work(state, 0)
  if (report !== null) writeFileSync(join(work, 'report.json'), report)
  f.sink.group(
    state,
    0,
    'group',
    ['f'],
    ['tsx'],
    f.dir,
    { status: 1, signal: null, stdout: 'DIFF x', stderr: '' },
    work,
  )
  writeFileSync(f.source, 'before')
  f.sink.end(state, 'CAUGHT', 'DIFF x')
}

test('retains separate reports and refuses an existing evidence directory', (t) => {
  const f = fixture(t)
  assert.equal(read(join(f.directory, 'catalogs/0.json')).length, 2)
  assert.notEqual(f.catalogBytes.length, 2)
  mutation(f, 0, '{"value":1}')
  mutation(f, 1, '{"value":2}')
  assert.equal(f.sink.finish('results'), true)
  assert.equal(
    readFileSync(join(f.directory, 'mutations/0-0/groups/0-report.raw.json'), 'utf8'),
    '{"value":1}',
  )
  assert.equal(
    readFileSync(join(f.directory, 'mutations/0-1/groups/0-report.raw.json'), 'utf8'),
    '{"value":2}',
  )
  assert.equal(read(join(f.directory, 'producer-finished.json')).records.length, 2)
  assert.throws(() => createMutationEvidence(f.options), /EEXIST/)
})

test('missing and corrupt reports leave records incomplete', (t) => {
  for (const report of [null, '{bad']) {
    const f = fixture(t)
    mutation(f, 0, report)
    assert.equal(f.sink.finish('results'), false)
    assert.equal(existsSync(join(f.directory, 'mutations/0-0/end.json')), false)
    assert.equal(existsSync(join(f.directory, 'producer-finished.json')), false)
    assert.ok(read(join(f.directory, 'capture-errors.json')).length > 0)
  }
})

test('duplicate mutation and failed restoration route errors separately', (t) => {
  const f = fixture(t)
  mutation(f, 0)
  const start = readFileSync(join(f.directory, 'mutations/0-0/start.json'))
  const duplicate = f.sink.begin(f.catalog, 0, f.fault, f.source, 'before', true, 1)
  writeFileSync(f.source, 'wrong')
  f.sink.end(duplicate, 'BUILD', 'native failure')
  assert.deepEqual(readFileSync(join(f.directory, 'mutations/0-0/start.json')), start)
  assert.equal(f.sink.finish('results'), false)
  assert.ok(
    read(join(f.directory, 'capture-errors.json')).some((error) => error.operation === 'end'),
  )
})

test('source read failure is an operational error, not a finished record', (t) => {
  const f = fixture(t)
  const state = f.sink.begin(f.catalog, 0, f.fault, f.source, 'before', true, 1)
  rmSync(f.source)
  f.sink.applied(state)
  writeFileSync(f.source, 'before')
  f.sink.end(state, 'BUILD', 'native failure')
  assert.equal(f.sink.finish('results'), false)
  assert.equal(existsSync(join(f.directory, 'mutations/0-0/end.json')), false)
  assert.ok(
    read(join(f.directory, 'capture-errors.json')).some((error) => error.operation === 'applied'),
  )
})

test('unavailable work directory never reads a preexisting report', (t) => {
  const f = fixture(t)
  const state = f.sink.begin(f.catalog, 0, f.fault, f.source, 'before', true, 1)
  writeFileSync(f.source, 'after')
  f.sink.applied(state)
  const stale = join(f.directory, 'work/0-0/0')
  mkdirSync(stale, { recursive: true })
  writeFileSync(join(stale, 'report.json'), '{"stale":true}')
  const work = f.sink.work(state, 0)
  assert.equal(work, undefined)
  f.sink.group(
    state,
    0,
    'group',
    ['f'],
    ['tsx'],
    f.dir,
    { status: 1, signal: null, stdout: 'DIFF x', stderr: '' },
    work,
  )
  writeFileSync(f.source, 'before')
  f.sink.end(state, 'CAUGHT', 'DIFF x')
  assert.equal(f.sink.finish('results'), false)
  const command = read(join(f.directory, 'mutations/0-0/groups/0-command.json'))
  assert.equal(command.reportSha256, null)
  assert.equal(existsSync(join(f.directory, 'mutations/0-0/groups/0-report.raw.json')), false)
  assert.ok(
    read(join(f.directory, 'capture-errors.json')).some((error) => error.operation === 'group'),
  )
})

test('comparison subprocess is not started when isolated work allocation fails', (t) => {
  const f = fixture(t)
  const shared = join(f.dir, 'shared')
  mkdirSync(shared)
  const sharedReport = join(shared, 'report.json')
  writeFileSync(sharedReport, '{"original":true}')
  const state = f.sink.begin(f.catalog, 0, f.fault, f.source, 'before', true, 1)
  writeFileSync(f.source, 'after')
  f.sink.applied(state)
  const args = [
    '-e',
    "require('node:fs').writeFileSync(require('node:path').join(process.env.MAIN_DIFF_OUT, 'report.json'), '{\"child\":true}')",
  ]
  const options = {
    evidence: f.sink,
    mutation: state,
    position: 0,
    group: 'group',
    functions: ['f'],
    command: process.execPath,
    args,
    cwd: f.dir,
    env: { ...process.env, MAIN_DIFF_OUT: shared },
  }
  const run = runComparison(options)
  assert.equal(run.status, 0)
  assert.equal(readFileSync(join(f.directory, 'work/0-0/0/report.json'), 'utf8'), '{"child":true}')
  assert.equal(readFileSync(sharedReport, 'utf8'), '{"original":true}')
  const blocked = runComparison(options)
  assert.equal(blocked, null)
  assert.equal(readFileSync(sharedReport, 'utf8'), '{"original":true}')
  writeFileSync(f.source, 'before')
  f.sink.end(state, 'CAUGHT', 'DIFF x')
  assert.equal(f.sink.finish('results'), false)
  assert.equal(existsSync(join(f.directory, 'mutations/0-0/end.json')), false)
})
