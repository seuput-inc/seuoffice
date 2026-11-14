import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'
import {
  auditPrerequisite,
  auditWitness,
  nextIdentity,
  nextPlan,
} from './next-core-sensitivity.mjs'

const manifest = JSON.parse(
  readFileSync(new URL('./next-core-sensitivity-manifest.json', import.meta.url)),
)
const definition = 'a'.repeat(40)
const env = {
  MODE: 'next-core-retained-sensitivity',
  PRODUCT_SHA: manifest.productSha,
  DEFINITION_SHA: definition,
  GITHUB_RUN_ID: '31',
  GITHUB_RUN_ATTEMPT: '1',
}
const clone = () => structuredClone(manifest)

function prerequisite() {
  const gate = manifest.prerequisite
  const run = {
    id: Number(gate.runId),
    run_attempt: gate.attempt,
    head_sha: gate.definitionSha,
    head_branch: gate.definitionRef,
    status: 'completed',
    conclusion: 'success',
  }
  const jobs = gate.requiredJobs.map((name) => ({
    name,
    status: 'completed',
    conclusion: 'success',
    steps: [{ name: 'Producer and audit', conclusion: 'success' }],
  }))
  const artifacts = gate.requiredArtifacts.map((name, index) => ({
    id: index + 1,
    name,
    expired: false,
    size_in_bytes: 100,
    digest: 'sha256:' + 'b'.repeat(64),
    workflow_run: { id: run.id, head_sha: run.head_sha },
  }))
  return { run, jobs, artifacts }
}

function witnessFixture(key = 'provider-7') {
  const job = manifest.jobs.find((entry) => entry.key === key)
  const difference = {
    fn: job.witness.fn,
    case: job.witness.case,
    typescript: job.witness.typescript,
    rust: job.witness.rust,
  }
  const report = { differences: [difference] }
  return {
    job,
    difference,
    report,
    retained: () => ({
      records: [
        { index: job.targetIndex, groups: [{ reportRaw: Buffer.from(JSON.stringify(report)) }] },
      ],
    }),
  }
}

test('explicit identity rejects another product, mode, mixed engine selection and a malformed attempt', () => {
  assert.deepEqual(nextIdentity(env, manifest), {
    productSha: manifest.productSha,
    definitionSha: definition,
    runId: '31',
    attempt: 1,
  })
  for (const change of [
    { MODE: 'main-retained-producer-proof' },
    { PRODUCT_SHA: manifest.parentProductSha },
    { MAIN_FAULTS: 'catalog' },
    { ALL_FAULTS: 'true' },
    { GITHUB_RUN_ATTEMPT: '0' },
  ])
    assert.throws(() => nextIdentity({ ...env, ...change }, manifest))
})

test('the 24 targets preserve their original indices, four-way lineage and all 47 records', () => {
  const plan = nextPlan(manifest, definition)
  assert.equal(plan.jobs.length, 24)
  assert.equal(
    plan.jobs.reduce((sum, job) => sum + job.indices.length, 0),
    47,
  )
  assert.equal(
    plan.jobs.reduce(
      (sum, job) => sum + job.baselineExpected.reduce((n, fn) => n + fn.checks, 0),
      0,
    ),
    2129,
  )
  assert.equal(plan.fullRecoveryAccepted, false)
  assert.equal(plan.currentProductReuseAccepted, false)
  for (const job of plan.jobs) {
    assert.ok(job.indices.length <= 2)
    assert.equal(job.shard % 4, job.targetIndex % 4)
  }
  assert.deepEqual(plan.jobs.find((job) => job.key === 'core-0').indices, [0])
})

test('a missing, duplicated, renumbered or broadened target plan fails closed', () => {
  for (const change of [
    (m) => m.jobs.pop(),
    (m) => {
      m.jobs[0] = m.jobs[1]
    },
    (m) => {
      m.jobs[0].targetIndex++
    },
    (m) => {
      m.jobs[0].indices.push(3)
    },
    (m) => {
      m.jobs[0].shards = 4
    },
    (m) => {
      m.jobs[0].ledgerRequirements = [{ id: 'unexpected' }]
    },
    (m) => {
      m.maxParallel = 9
    },
  ]) {
    const value = clone()
    change(value)
    assert.throws(() => nextPlan(value, definition))
  }
})

test('a source-predicted whole wire is mandatory, not merely a CAUGHT diagnostic', () => {
  const f = witnessFixture()
  assert.deepEqual(auditWitness(f.job, f.retained()), f.difference)
  f.report.differences = []
  assert.throws(() => auditWitness(f.job, f.retained()), /typed consumer witness/)
})

test('a different selector, case, TS oracle or Rust field cannot satisfy the witness', () => {
  for (const change of [
    (f) => {
      f.difference.fn = 'unrelated.fn'
    },
    (f) => {
      f.difference.case++
    },
    (f) => {
      f.difference.typescript = f.difference.rust
    },
    (f) => {
      f.difference.rust = f.difference.typescript
    },
    (f) => {
      const wire = JSON.parse(f.difference.rust)
      wire.files.extra = 'unexpected'
      f.difference.rust = JSON.stringify(wire)
    },
  ]) {
    const f = witnessFixture()
    change(f)
    assert.throws(() => auditWitness(f.job, f.retained()))
  }
})

test('duplicate witness tuples and malformed answer wires remain rejected', () => {
  const f = witnessFixture()
  f.report.differences.push(f.difference)
  assert.throws(() => auditWitness(f.job, f.retained()), /duplicate/)
  const bad = witnessFixture()
  bad.difference.rust = JSON.stringify({ answer: 'not JSON', files: {} })
  assert.throws(() => auditWitness(bad.job, bad.retained()))
})

test('corrected control0 and failed-build metadata do not become original successes', () => {
  for (const key of ['core-0', 'provider-27', 'skills-131']) {
    const f = witnessFixture(key)
    assert.deepEqual(auditWitness(f.job, f.retained()), f.difference)
  }
  assert.equal(manifest.metadataLineage.length, 3)
  assert.ok(
    manifest.metadataLineage.every(
      (entry) => entry.oldMutationSha256 !== entry.mutationSha256 && entry.originalFailureUnchanged,
    ),
  )
})

test('all source-predicted target witnesses require exact independent whole-wire differences', () => {
  for (const job of manifest.jobs) {
    const f = witnessFixture(job.key)
    assert.notEqual(f.difference.typescript, f.difference.rust)
    const raw = f.retained()
    auditWitness(job, raw)
    raw.records[0].index = -1
    assert.throws(() => auditWitness(job, raw), /original record/)
  }
})

test('the exact completed prerequisite binds all three required jobs and artifacts', () => {
  const { run, jobs, artifacts } = prerequisite()
  const receipt = auditPrerequisite(manifest, run, jobs, artifacts)
  assert.equal(receipt.productSha, manifest.productSha)
  assert.equal(receipt.scope, 'exact-product-three-lane-prerequisite-not-legacy-fault-gate')
})

test('a green unrelated run, missing lane, failed producer or expired artifact cannot open sensitivity', () => {
  for (const change of [
    (f) => {
      f.run.head_sha = definition
    },
    (f) => {
      f.run.run_attempt++
    },
    (f) => {
      f.run.status = 'in_progress'
    },
    (f) => {
      f.run.conclusion = 'failure'
    },
    (f) => {
      f.jobs.pop()
    },
    (f) => {
      f.jobs[0].steps[0].conclusion = 'failure'
    },
    (f) => {
      f.artifacts[0].expired = true
    },
    (f) => {
      f.artifacts[0].workflow_run.id++
    },
    (f) => {
      f.artifacts.push(f.artifacts[0])
    },
  ]) {
    const f = prerequisite()
    change(f)
    assert.throws(() => auditPrerequisite(manifest, f.run, f.jobs, f.artifacts))
  }
})
