import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import {
  auditPrerequisite,
  auditWitness,
  finalIdentity,
  finalPlan,
} from './final-core-sensitivity.mjs'

const manifest = JSON.parse(
  readFileSync(new URL('./final-core-sensitivity-manifest.json', import.meta.url)),
)
const definition = 'a'.repeat(40)
const env = {
  MODE: 'final-core-retained-sensitivity',
  PRODUCT_SHA: manifest.productSha,
  DEFINITION_SHA: definition,
  GITHUB_RUN_ID: '17',
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
    steps: gate.requiredSteps[name].map((name) => ({ name, conclusion: 'success' })),
  }))
  const artifacts = gate.requiredArtifacts.map((name, index) => ({
    name,
    id: index + 1,
    expired: false,
    size_in_bytes: 1,
    digest: `sha256:${'1'.repeat(64)}`,
    workflow_run: { id: run.id, head_sha: run.head_sha },
  }))
  return { run, jobs, artifacts }
}
function witness(job) {
  return {
    records: [
      {
        index: job.targetIndex,
        groups: [
          {
            reportRaw: JSON.stringify({
              differences: [
                {
                  fn: job.witness.fn,
                  case: job.witness.case,
                  typescript: job.witness.typescript,
                  rust: job.witness.rust,
                },
              ],
            }),
          },
        ],
      },
    ],
  }
}

test('explicit mode rejects another product and all mixed legacy selections', () => {
  assert.equal(finalIdentity(env, manifest).productSha, manifest.productSha)
  for (const patch of [
    { MODE: 'next-core-retained-sensitivity' },
    { PRODUCT_SHA: manifest.parentProductSha },
    { ALL_FAULTS: 'true' },
    { MAIN_FAULTS: 'other.json' },
    { BASELINE_PRODUCT_SHA: manifest.productSha },
    { FAULT_PRODUCT_SHA: manifest.productSha },
    { GATE_RUN_ID: '1' },
    { DOCX_FAULTS: 'other.json' },
    { ODT_FAULTS: '1' },
    { GITHUB_RUN_ATTEMPT: '0' },
  ])
    assert.throws(() => finalIdentity({ ...env, ...patch }, manifest))
})

test('ten original noncontrols select twenty records with unchanged four-way lineage', () => {
  const plan = finalPlan(manifest, definition)
  assert.equal(plan.jobs.length, 10)
  assert.equal(
    plan.jobs.reduce((sum, job) => sum + job.indices.length, 0),
    20,
  )
  assert.equal(
    plan.jobs.reduce(
      (sum, job) => sum + job.baselineExpected.reduce((n, fn) => n + fn.checks, 0),
      0,
    ),
    595,
  )
  assert.equal(plan.maxParallel, 8)
  assert.equal(plan.timeoutMinutes, 120)
  for (const job of plan.jobs) {
    assert.deepEqual(job.indices, [0, job.targetIndex])
    assert.equal(job.shard % 4, job.targetIndex % 4)
    assert.equal(job.shards % 4, 0)
  }
})

test('a missing duplicate broadened or renumbered target remains rejected', () => {
  for (const change of [
    (m) => m.jobs.pop(),
    (m) => {
      m.jobs[1] = m.jobs[0]
    },
    (m) => {
      m.jobs[0].indices.push(1)
    },
    (m) => {
      m.jobs[0].entries[1].index += 1
    },
    (m) => {
      m.jobs[0].shards += 1
    },
    (m) => {
      m.jobs[0].witness.case = 999999
    },
    (m) => {
      delete m.jobs[0].baselineExpected[0].group
    },
    (m) => {
      delete m.jobs[0].baselineExpected[0].single
    },
    (m) => {
      m.prerequisite.productSha = m.parentProductSha
    },
    (m) => {
      m.jobs[0].ledgerRequirements = [{ id: 'unregistered' }]
    },
    (m) => {
      m.fullRecoveryAccepted = true
    },
  ]) {
    const altered = clone()
    change(altered)
    assert.throws(() => finalPlan(altered, definition))
  }
})

test('an exact consumer wire is mandatory for every original target', () => {
  for (const job of manifest.jobs) {
    const actual = auditWitness(job, witness(job))
    assert.equal(actual.typescript, job.witness.typescript)
    assert.equal(actual.rust, job.witness.rust)
    for (const patch of [
      { fn: 'another.fn' },
      { case: job.witness.case + 1 },
      { typescript: job.witness.rust },
      { rust: job.witness.typescript },
      { rust: JSON.stringify({ answer: '{}', files: {} }) },
    ]) {
      const retained = witness(job)
      const report = JSON.parse(retained.records[0].groups[0].reportRaw)
      Object.assign(report.differences[0], patch)
      retained.records[0].groups[0].reportRaw = JSON.stringify(report)
      assert.throws(() => auditWitness(job, retained))
    }
  }
})

test('missing or duplicate typed differences cannot be replaced by a CAUGHT diagnostic', () => {
  const job = manifest.jobs[0]
  assert.throws(() => auditWitness(job, { records: [] }))
  const retained = witness(job)
  const report = JSON.parse(retained.records[0].groups[0].reportRaw)
  report.differences.push(report.differences[0])
  retained.records[0].groups[0].reportRaw = JSON.stringify(report)
  assert.throws(() => auditWitness(job, retained))
  retained.records[0].groups[0].reportRaw = JSON.stringify({
    differences: [],
    diagnostic: 'CAUGHT',
  })
  assert.throws(() => auditWitness(job, retained))
})

test('the latest product requires its own completed three-lane run', () => {
  const f = prerequisite()
  const receipt = auditPrerequisite(manifest, f.run, f.jobs, f.artifacts)
  assert.equal(receipt.productSha, manifest.productSha)
  assert.equal(receipt.runId, '43661410435')
  for (const patch of [
    { status: 'in_progress', conclusion: null },
    { conclusion: 'failure' },
    { head_sha: manifest.parentProductSha },
    { head_branch: 'ci/stage2-hosted-prepush-baseline' },
    { id: 43648349140 },
    { run_attempt: 2 },
  ])
    assert.throws(() => auditPrerequisite(manifest, { ...f.run, ...patch }, f.jobs, f.artifacts))
})

test('mandatory native steps must actually succeed and cannot be absent duplicated or skipped', () => {
  for (const name of manifest.prerequisite.requiredJobs) {
    for (const change of [
      (job) => {
        job.steps.pop()
      },
      (job) => {
        job.steps[0].conclusion = 'skipped'
      },
      (job) => {
        job.steps[0].conclusion = 'failure'
      },
      (job) => {
        job.steps.push(job.steps[0])
      },
    ]) {
      const f = prerequisite()
      change(f.jobs.find((job) => job.name === name))
      assert.throws(() => auditPrerequisite(manifest, f.run, f.jobs, f.artifacts))
    }
  }
})

test('another expired missing or extra artifact cannot satisfy the prerequisite', () => {
  for (const change of [
    (f) => {
      f.jobs.pop()
    },
    (f) => {
      f.artifacts.pop()
    },
    (f) => {
      f.artifacts.push(f.artifacts[0])
    },
    (f) => {
      f.artifacts[0].expired = true
    },
    (f) => {
      f.artifacts[0].workflow_run.head_sha = 'b'.repeat(40)
    },
  ]) {
    const f = prerequisite()
    change(f)
    assert.throws(() => auditPrerequisite(manifest, f.run, f.jobs, f.artifacts))
  }
})
