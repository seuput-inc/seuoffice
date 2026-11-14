import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import {
  bindReceiptCorpus,
  canonicalObjectSha256,
  assertPrepushSucceeded,
  auditBaseline,
  identity,
  validateReuseProposal,
  validateSourceObservation,
} from './hosted-validation.mjs'

const manifest = JSON.parse(
  readFileSync(new URL('./hosted-validation-manifest.json', import.meta.url)),
)
const hash = (value) => createHash('sha256').update(JSON.stringify(value)).digest('hex')
const env = {
  DEFINITION_SHA: 'a'.repeat(40),
  VALIDATION_PRODUCT_SHA: manifest.productSha,
  GITHUB_RUN_ID: '19',
  GITHUB_RUN_ATTEMPT: '1',
  PROFILE: 'linux',
}

function baselineFixture() {
  const item = { group: 'docs', name: 'api.read', checks: 2, single: false }
  const fixed = { ...manifest, inventory: { linux: [item] } }
  const plan = identity(env, fixed)
  const inventory = { linux: [item] }
  const reports = {
    linux: {
      docs: {
        report: { 'api.read': { checks: 2, equal: 2, listed: 0, distinct: 2 } },
        differences: [],
      },
    },
  }
  const receipt = {
    ...plan,
    contract: 'main-baseline-only/v1',
    comparisonVerdict: 'success',
    totals: { linux: { functions: 1, checks: 2 } },
  }
  return { fixed, plan, inventory, reports, receipt }
}

test('an exact product identity still rejects a mixed legacy or fault request', () => {
  for (const key of ['BASELINE_PRODUCT_SHA', 'MAIN_FAULTS', 'FAULT_PRODUCT_SHA', 'GATE_RUN_ID']) {
    assert.throws(() => identity({ ...env, [key]: 'unexpected' }, manifest), /conflicts/)
  }
  assert.throws(() => identity({ ...env, ALL_FAULTS: 'true' }, manifest), /conflicts/)
  assert.throws(() => identity({ ...env, FAULT_SHARDS: '4' }, manifest), /no fault shards/)
  assert.throws(
    () => identity({ ...env, VALIDATION_PRODUCT_SHA: 'b'.repeat(40) }, manifest),
    /Wrong fixed/,
  )
})

test('a different source range, base or prepush script cannot validate the pinned product', () => {
  const observation = {
    head: manifest.productSha,
    baseAncestor: manifest.baseSha,
    trackedChanges: '',
    range: manifest.sourceRange,
    prepushScriptSha256: manifest.prepushScriptSha256,
    registrySha256: manifest.registrySha256,
  }
  assert.throws(
    () =>
      validateSourceObservation(manifest, { ...observation, baseAncestor: manifest.productSha }),
    /merge base/,
  )
  assert.throws(
    () =>
      validateSourceObservation(manifest, { ...observation, range: observation.range.slice(1) }),
    /source range/,
  )
  assert.throws(
    () => validateSourceObservation(manifest, { ...observation, trackedChanges: 'Cargo.lock' }),
    /Source changed/,
  )
  assert.throws(
    () =>
      validateSourceObservation(manifest, { ...observation, prepushScriptSha256: '0'.repeat(64) }),
    /Prepush changed/,
  )
})

test('a failed, signaled or unstarted prepush cannot produce a successful receipt', () => {
  for (const result of [
    { status: 17, signal: null, error: null },
    { status: null, signal: 'SIGTERM', error: null },
    { status: null, signal: null, error: { code: 'ENOENT' } },
  ])
    assert.throws(() => assertPrepushSucceeded(result))
})

test('a fixed complete baseline yields counts but never grants fault or reuse acceptance', () => {
  const { fixed, plan, inventory, reports, receipt } = baselineFixture()
  assert.deepEqual(auditBaseline(plan, fixed, inventory, reports, receipt), {
    linux: { functions: 1, checks: 2 },
  })
  assert.equal(plan.reuseAccepted, false)
  assert.equal(plan.legacyFaultGateCompatible, false)
})

test('a green old baseline with fewer cases cannot stand in for the new fixed inventory', () => {
  const { fixed, plan, inventory, reports, receipt } = baselineFixture()
  const old = structuredClone(inventory)
  old.linux[0].checks = 1
  assert.throws(() => auditBaseline(plan, fixed, old, reports, receipt), /fixed product inventory/)
  reports.linux.docs.report['api.read'].checks = 1
  reports.linux.docs.report['api.read'].equal = 1
  assert.throws(
    () => auditBaseline(plan, fixed, inventory, reports, receipt),
    /Missing or duplicated cases/,
  )
})

test('unlisted differences, missing functions, FLOOR and another definition stay failures', () => {
  for (const change of [
    ({ reports }) =>
      reports.linux.docs.differences.push({
        fn: 'api.read',
        case: 1,
        typescript: 'before',
        rust: 'after',
      }),
    ({ reports }) => {
      delete reports.linux.docs.report['api.read']
    },
    ({ reports }) => {
      reports.linux.docs.report['api.read'].distinct = 1
    },
    ({ receipt }) => {
      receipt.definitionSha = 'b'.repeat(40)
    },
  ]) {
    const f = baselineFixture()
    change(f)
    assert.throws(() => auditBaseline(f.plan, f.fixed, f.inventory, f.reports, f.receipt))
  }
})

function reuseFixture() {
  const path = 'tools/main-diff/faults/docs.json'
  const faults = [
    { name: 'control', file: 'main-core/a.rs', find: 'a', replace: 'b', fn: ['api.read'] },
    { name: 'other', file: 'main-core/a.rs', find: 'c', replace: 'd', fn: ['api.read'] },
  ]
  const catalogs = new Map([[path, { faults, sha256: hash(faults) }]])
  const proposal = {
    productSha: manifest.productSha,
    currentProductReuseAccepted: false,
    dispatchAuthorized: false,
    oldFailuresUnchanged: true,
    requiredIds: [`${path}#1`],
    eligibleReuseCandidates: [{ id: `${path}#0`, mutationSha256: hash(faults[0]) }],
    jobs: [
      {
        catalog: path,
        indices: [0, 1],
        entryHashes: faults.map(hash),
      },
    ],
  }
  return {
    catalogs,
    proposal,
    fixed: {
      ...manifest,
      reuseProposalSha256: hash(proposal),
      catalogSha256: { [path]: hash(faults) },
    },
    path,
  }
}

test('a proposal without per-job raw hashes binds its fixed catalog bytes', () => {
  const f = reuseFixture()
  validateReuseProposal(f.fixed, f.proposal, f.catalogs)
})

test('an extra mutation or changed candidate object is not hidden by the reuse proposal', () => {
  const extra = reuseFixture()
  extra.catalogs.get(extra.path).faults.push({ name: 'extra' })
  assert.throws(
    () => validateReuseProposal(extra.fixed, extra.proposal, extra.catalogs),
    /Incomplete ID universe/,
  )
  const changed = reuseFixture()
  changed.catalogs.get(changed.path).faults[0].replace = 'different'
  assert.throws(
    () => validateReuseProposal(changed.fixed, changed.proposal, changed.catalogs),
    /Wrong candidate object/,
  )
})

test('canonical fault objects do not excuse a wrong raw catalog hash', () => {
  const f = reuseFixture()
  f.catalogs.get(f.path).sha256 = '0'.repeat(64)
  assert.throws(() => validateReuseProposal(f.fixed, f.proposal, f.catalogs), /Wrong catalog bytes/)
})

test('canonical mutation hashes preserve UTF-16 JSON escapes, UTF-8 and selector order', () => {
  const fault = { name: '한글', find: '\ud83d', replace: '\n', fn: ['api.a', 'api.b'] }
  const bytes = Buffer.from(
    '{"name":"한글","find":"\\ud83d","replace":"\\n","fn":["api.a","api.b"]}',
    'utf8',
  )
  assert.equal(canonicalObjectSha256(fault), createHash('sha256').update(bytes).digest('hex'))
  assert.notEqual(
    canonicalObjectSha256(fault),
    canonicalObjectSha256({ ...fault, fn: ['api.b', 'api.a'] }),
  )
})

test('the fixed historical corpus binds actual completed IDs but grants no current reuse', () => {
  const proposal = JSON.parse(
    readFileSync(new URL('./hosted-validation-reuse.json', import.meta.url)),
  )
  const corpus = JSON.parse(
    readFileSync(new URL('./hosted-validation-receipts.json', import.meta.url)),
  )
  const binding = bindReceiptCorpus(manifest, proposal, corpus)
  assert.equal(binding.candidateBindings, 2764)
  assert.equal(binding.receiptBodies, 7)
  assert.equal(binding.currentProductReuseAccepted, false)
  const withoutRows = structuredClone(corpus)
  withoutRows.strictRecoverySnapshot.accepted = []
  const changedManifest = { ...manifest, receiptCorpusSha256: canonicalObjectSha256(withoutRows) }
  assert.throws(() => bindReceiptCorpus(changedManifest, proposal, withoutRows))
})

test('the real CLI rejects mixed input before writing a plan or starting a native command', () => {
  const run = spawnSync(
    process.execPath,
    [fileURLToPath(new URL('./hosted-validation.mjs', import.meta.url)), 'prepare'],
    {
      env: {
        ...process.env,
        ...env,
        ALL_FAULTS: 'true',
        VALIDATION_PLAN_PATH: fileURLToPath(
          new URL('./hosted-validation-manifest.json', import.meta.url),
        ),
      },
      encoding: 'utf8',
    },
  )
  assert.equal(run.status, 1)
  assert.match(run.stderr, /ALL_FAULTS conflicts with validation/)
  assert.equal(run.stdout, '')
})
