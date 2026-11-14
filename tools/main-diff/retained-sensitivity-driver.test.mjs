import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, symlinkSync } from 'node:fs'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import {
  proofIdentity,
  retainedPlan,
  loadRetainedEvidence,
} from './retained-sensitivity-driver.mjs'

const product = 'a'.repeat(40)
const definition = 'b'.repeat(40)
const digest = 'c'.repeat(64)
const manifest = () => ({
  contract: 'retained-sensitivity-manifest/v1',
  scope: 'retained-producer-bounded-proof-only',
  productSha: product,
  parentProductSha: 'd'.repeat(40),
  catalog: 'tools/main-diff/faults/example.json',
  catalogSha256: digest,
  catalogLength: 17,
  shard: 1,
  shards: 4,
  indices: [0, 1, 5, 9, 13],
  focus: {
    index: 5,
    fn: 'example.read',
    ledgerCase: 0,
    independentCase: 2,
    typescript: '{}',
    rust: '{"different":true}',
  },
  modules: {
    'tools/main-diff/faults.mjs': digest,
    'tools/main-diff/mutation-evidence.mjs': digest,
  },
  entries: [0, 1, 5, 9, 13].map((index) => ({ index, mutationSha256: digest })),
  baselineExpected: [{ group: 'example', name: 'example.read', checks: 3, single: false }],
  ledgerRequirements: [],
  sourceLineage: [
    {
      path: 'tools/main-diff/faults.mjs',
      parentSha256: digest,
      productSha256: digest,
    },
  ],
})

const environment = () => ({
  MODE: 'main-retained-producer-proof',
  PRODUCT_SHA: product,
  DEFINITION_SHA: definition,
  GITHUB_RUN_ID: '42',
  GITHUB_RUN_ATTEMPT: '1',
})

test('pins explicit mode, exact product and separate definition identity', () => {
  assert.deepEqual(proofIdentity(environment(), manifest()), {
    productSha: product,
    definitionSha: definition,
    runId: '42',
    attempt: 1,
  })
  for (const change of [
    { MODE: 'main-ledger-consumer-suite' },
    { PRODUCT_SHA: definition },
    { DEFINITION_SHA: 'HEAD' },
    { GITHUB_RUN_ATTEMPT: '0' },
  ])
    assert.throws(() => proofIdentity({ ...environment(), ...change }, manifest()))
})

test('keeps original residue, control and a non-terminal typed focus', () => {
  const plan = retainedPlan(manifest(), definition)
  assert.equal(plan.contract, 'retained-sensitivity-plan/v1')
  assert.deepEqual(plan.indices, [0, 1, 5, 9, 13])
  assert.equal(plan.indices.at(-1) > plan.focus.index, true)
  for (const change of [
    { indices: [0, 1, 5, 13] },
    { indices: [0, 1, 5, 9, 13, 13] },
    { shards: 3 },
    { shard: 4 },
    { focus: { ...manifest().focus, index: 13 } },
    { catalog: '../outside.json' },
    { modules: { ...manifest().modules, fallback: digest } },
    {
      catalogLength: 101,
      indices: Array.from({ length: 101 }, (_, i) => i).filter((i) => i === 0 || i % 4 === 1),
    },
  ])
    assert.throws(() => retainedPlan({ ...manifest(), ...change }, definition))
})

function evidenceFixture(t) {
  const root = mkdtempSync(join(process.env.OMPDESK_SCRATCH_DIR ?? tmpdir(), 'retained-loader-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  const directory = join(root, 'evidence')
  mkdirSync(directory)
  const plan = {
    indices: [0, 1],
    faults: [{ fn: ['example.read'] }, { fn: ['example.read'] }],
  }
  const baseline = { functions: [{ name: 'example.read', group: 'example' }] }
  const files = ['run.json', 'producer-finished.json', 'catalogs/0.json']
  for (const index of plan.indices)
    files.push(
      ...[
        'start.json',
        'applied.json',
        'end.json',
        'groups/0-command.json',
        'groups/0-report.raw.json',
      ].map((path) => `mutations/0-${index}/${path}`),
    )
  for (const path of files) {
    const target = join(directory, path)
    mkdirSync(join(target, '..'), { recursive: true })
    writeFileSync(target, `{"path":${JSON.stringify(path)}}`)
  }
  return { root, directory, plan, baseline }
}

test('loads every original report rather than a terminal/default report', (t) => {
  const f = evidenceFixture(t)
  const value = loadRetainedEvidence(f.directory, f.plan, f.baseline)
  assert.equal(value.records.length, 2)
  assert.notDeepEqual(value.records[0].groups[0].reportRaw, value.records[1].groups[0].reportRaw)
  rmSync(join(f.directory, 'mutations/0-0/groups/0-report.raw.json'))
  writeFileSync(join(f.root, 'report.json'), '{}')
  assert.throws(() => loadRetainedEvidence(f.directory, f.plan, f.baseline))
})

test('rejects operational capture errors, symbolic files and symbolic directories', (t) => {
  const f = evidenceFixture(t)
  writeFileSync(join(f.directory, 'capture-errors.json'), '[]')
  assert.throws(() => loadRetainedEvidence(f.directory, f.plan, f.baseline), /capture errors/)
  rmSync(join(f.directory, 'capture-errors.json'))
  symlinkSync(join(f.root, 'missing.json'), join(f.directory, 'capture-errors.json'))
  assert.throws(() => loadRetainedEvidence(f.directory, f.plan, f.baseline), /capture errors/)
  rmSync(join(f.directory, 'capture-errors.json'))
  const path = join(f.directory, 'mutations/0-0/groups/0-report.raw.json')
  rmSync(path)
  writeFileSync(join(f.root, 'outside.json'), '{}')
  symlinkSync(join(f.root, 'outside.json'), path)
  assert.throws(
    () => loadRetainedEvidence(f.directory, f.plan, f.baseline),
    /Symbolic evidence path/,
  )
  rmSync(path)
  rmSync(join(f.directory, 'mutations/0-0/groups'), { recursive: true })
  symlinkSync(f.root, join(f.directory, 'mutations/0-0/groups'))
  assert.throws(
    () => loadRetainedEvidence(f.directory, f.plan, f.baseline),
    /Symbolic evidence path/,
  )
})
