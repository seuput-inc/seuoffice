import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  appendFileSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs'
import { isAbsolute, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { auditRecordedRun } from './mutation-evidence-audit.mjs'
import { loadRetainedEvidence, runSelectedBaseline } from './retained-sensitivity-driver.mjs'

const self = fileURLToPath(import.meta.url)
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex')
export const canonicalMutationObjectHash = (value) => hash(JSON.stringify(value))
const json = (path) => JSON.parse(readFileSync(path, 'utf8'))
const save = (path, value) =>
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, { flag: 'wx' })
const git = (root, ...args) => execFileSync('git', ['-C', root, ...args])
const sha = (value) => typeof value === 'string' && /^[a-f0-9]{40}$/.test(value)
const digest = (value) => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value)
const safe = (path) =>
  typeof path === 'string' &&
  path.length > 0 &&
  !isAbsolute(path) &&
  !path.split(/[\\/]/).some((part) => !part || part === '.' || part === '..')
const emit = (env, name, value) => {
  if (env.GITHUB_OUTPUT) appendFileSync(env.GITHUB_OUTPUT, `${name}=${value}\n`)
}

export function nextIdentity(env, manifest) {
  assert.equal(env.MODE, 'next-core-retained-sensitivity')
  assert.ok(sha(env.DEFINITION_SHA) && sha(env.PRODUCT_SHA))
  assert.equal(env.PRODUCT_SHA, manifest.productSha)
  for (const key of [
    'MAIN_FAULTS',
    'FAULT_PRODUCT_SHA',
    'GATE_RUN_ID',
    'BASELINE_PRODUCT_SHA',
    'DOCX_FAULTS',
    'ODT_FAULTS',
  ]) {
    assert.equal((env[key] ?? '').trim(), '', `${key} conflicts with next-core sensitivity`)
  }
  assert.ok(
    ['', 'false'].includes(env.ALL_FAULTS ?? ''),
    'ALL_FAULTS conflicts with next-core sensitivity',
  )
  assert.match(env.GITHUB_RUN_ID ?? '', /^[1-9][0-9]*$/)
  assert.match(env.GITHUB_RUN_ATTEMPT ?? '', /^[1-9][0-9]*$/)
  assert.ok(Number.isSafeInteger(Number(env.GITHUB_RUN_ATTEMPT)))
  return {
    productSha: env.PRODUCT_SHA,
    definitionSha: env.DEFINITION_SHA,
    runId: env.GITHUB_RUN_ID,
    attempt: Number(env.GITHUB_RUN_ATTEMPT),
  }
}

export function nextPlan(manifest, definitionSha) {
  assert.equal(manifest.contract, 'next-core-sensitivity-manifest/v1')
  assert.equal(manifest.scope, 'original-21-and-corrected-3-retained-sensitivity-only')
  assert.equal(manifest.canonicalObjectHashContract, 'sha256-json-stringify-utf8/v1')
  assert.ok(sha(definitionSha) && sha(manifest.productSha) && sha(manifest.parentProductSha))
  assert.notEqual(manifest.productSha, manifest.parentProductSha)
  assert.equal(manifest.prerequisite.productSha, manifest.productSha)
  assert.ok(sha(manifest.prerequisite.definitionSha))
  assert.deepEqual(manifest.prerequisite.requiredJobs, [
    'main-hosted-prepush',
    'main-baseline-only (ubuntu-latest, linux)',
    'main-baseline-only (windows-latest, windows)',
  ])
  assert.deepEqual(manifest.prerequisite.requiredArtifacts, [
    'main-validation-hosted-prepush',
    'main-validation-baseline-linux',
    'main-validation-baseline-windows',
  ])
  assert.equal(manifest.jobs.length, 24)
  assert.equal(new Set(manifest.jobs.map((job) => job.key)).size, 24)
  assert.equal(new Set(manifest.jobs.map((job) => `${job.catalog}#${job.targetIndex}`)).size, 24)
  assert.deepEqual(Object.keys(manifest.modules).sort(), [
    'tools/main-diff/faults.mjs',
    'tools/main-diff/mutation-evidence.mjs',
  ])
  assert.ok(Object.values(manifest.modules).every(digest))
  let records = 0
  for (const job of manifest.jobs) {
    assert.match(job.key, /^[a-z0-9-]+$/)
    assert.ok(safe(job.catalog) && digest(job.catalogSha256))
    assert.ok(Number.isSafeInteger(job.catalogLength) && job.catalogLength > 1)
    assert.ok(
      Number.isSafeInteger(job.targetIndex) &&
        job.targetIndex >= 0 &&
        job.targetIndex < job.catalogLength,
    )
    const shards = 4 * Math.ceil(job.catalogLength / 4)
    assert.equal(job.shards, shards)
    assert.equal(job.shard, job.targetIndex)
    assert.equal(job.shard % 4, job.targetIndex % 4)
    const indices = Array.from({ length: job.catalogLength }, (_, index) => index).filter(
      (index) => index === 0 || index % shards === job.shard,
    )
    assert.deepEqual(job.indices, indices)
    assert.deepEqual(indices, job.targetIndex ? [0, job.targetIndex] : [0])
    assert.equal(job.entries.length, indices.length)
    for (const [position, entry] of job.entries.entries()) {
      assert.equal(entry.index, indices[position])
      assert.ok(digest(entry.mutationSha256))
    }
    assert.ok(job.baselineExpected.length > 0)
    assert.equal(
      new Set(job.baselineExpected.map((fn) => fn.name)).size,
      job.baselineExpected.length,
    )
    assert.deepEqual(job.ledgerRequirements, [])
    assert.ok(job.witness && typeof job.witness.fn === 'string')
    const fn = job.baselineExpected.find((entry) => entry.name === job.witness.fn)
    assert.ok(
      fn &&
        Number.isSafeInteger(job.witness.case) &&
        job.witness.case >= 0 &&
        job.witness.case < fn.checks,
    )
    assert.equal(typeof job.witness.typescript, 'string')
    assert.equal(typeof job.witness.rust, 'string')
    assert.notEqual(job.witness.typescript, job.witness.rust)
    decodeWire(job.witness.typescript)
    decodeWire(job.witness.rust)
    records += indices.length
  }
  assert.equal(records, 47)
  assert.equal(manifest.maxParallel, 8)
  assert.equal(manifest.timeoutMinutes, 120)
  assert.equal(manifest.fullRecoveryAccepted, false)
  assert.equal(manifest.currentProductReuseAccepted, false)
  return {
    ...manifest,
    contract: 'next-core-sensitivity-plan/v1',
    definitionSha,
    manifestSha256: canonicalMutationObjectHash(manifest),
  }
}

function decodeWire(raw) {
  const wire = JSON.parse(raw)
  assert.ok(
    wire &&
      typeof wire.answer === 'string' &&
      wire.files &&
      typeof wire.files === 'object' &&
      !Array.isArray(wire.files),
  )
  const answer = JSON.parse(wire.answer)
  assert.ok(answer && typeof answer === 'object' && !Array.isArray(answer))
  assert.ok(Object.values(wire.files).every((file) => typeof file === 'string'))
  return { ...wire, answer }
}

export function auditWitness(job, retained) {
  const record = retained.records.find((entry) => entry.index === job.targetIndex)
  assert.ok(record, 'Missing target original record')
  const differences = record.groups.flatMap((group) => JSON.parse(group.reportRaw).differences)
  const selected = differences.filter(
    (entry) => entry.fn === job.witness.fn && entry.case === job.witness.case,
  )
  assert.equal(selected.length, 1, 'Missing or duplicate typed consumer witness')
  const difference = selected[0]
  decodeWire(difference.typescript)
  decodeWire(difference.rust)
  assert.equal(difference.typescript, job.witness.typescript, 'Wrong normal consumer wire')
  assert.equal(difference.rust, job.witness.rust, 'Wrong source-predicted mutant wire')
  return difference
}

function checkedFile(product, path) {
  assert.ok(safe(path))
  const raw = readFileSync(join(product, path))
  assert.deepEqual(raw, git(product, 'show', `HEAD:${path}`), `Changed product file ${path}`)
  return raw
}

function bindProduct(product, plan) {
  assert.equal(git(product, 'rev-parse', 'HEAD').toString().trim(), plan.productSha)
  assert.equal(git(product, 'diff', '--name-only', 'HEAD').toString().trim(), '')
  const paths = git(product, 'diff', '--name-only', plan.parentProductSha, plan.productSha)
    .toString()
    .trim()
    .split('\n')
    .filter(Boolean)
  assert.deepEqual(paths.sort(), plan.sourceLineage.map((entry) => entry.path).sort())
  for (const entry of plan.sourceLineage) {
    assert.ok(safe(entry.path) && digest(entry.productSha256))
    const present = git(product, 'ls-tree', plan.parentProductSha, '--', entry.path).length > 0
    assert.equal(
      present ? hash(git(product, 'show', `${plan.parentProductSha}:${entry.path}`)) : null,
      entry.parentSha256,
    )
    assert.equal(hash(checkedFile(product, entry.path)), entry.productSha256)
  }
  for (const [path, expected] of Object.entries(plan.modules))
    assert.equal(hash(checkedFile(product, path)), expected)
  const catalogs = new Map()
  for (const job of plan.jobs) {
    if (!catalogs.has(job.catalog)) {
      const raw = checkedFile(product, job.catalog)
      catalogs.set(job.catalog, { raw, faults: JSON.parse(raw) })
    }
    const catalog = catalogs.get(job.catalog)
    assert.equal(hash(catalog.raw), job.catalogSha256)
    assert.equal(catalog.faults.length, job.catalogLength)
    for (const entry of job.entries)
      assert.equal(canonicalMutationObjectHash(catalog.faults[entry.index]), entry.mutationSha256)
    const names = [...new Set(job.indices.flatMap((index) => catalog.faults[index].fn))].sort()
    assert.deepEqual(names, job.baselineExpected.map((entry) => entry.name).sort())
    assert.ok(catalog.faults[job.targetIndex].fn.includes(job.witness.fn))
    const ledger = json(join(product, 'tools/main-diff/divergences.json')).filter((entry) =>
      names.includes(entry.fn),
    )
    assert.deepEqual(ledger, job.ledgerRequirements)
  }
  assert.equal(plan.metadataLineage.length, 3)
  for (const lineage of plan.metadataLineage) {
    const beforeCatalog = JSON.parse(
      git(product, 'show', `${plan.parentProductSha}:${lineage.catalog}`),
    )
    const afterCatalog = catalogs.get(lineage.catalog).faults
    assert.equal(beforeCatalog.length, afterCatalog.length)
    const before = beforeCatalog[lineage.index],
      after = afterCatalog[lineage.index]
    assert.equal(canonicalMutationObjectHash(before), lineage.oldMutationSha256)
    assert.equal(canonicalMutationObjectHash(after), lineage.mutationSha256)
    assert.equal(before.name, lineage.oldName)
    assert.equal(after.name, lineage.name)
    assert.equal(before.replace, lineage.oldReplace)
    assert.equal(after.replace, lineage.replace)
    for (const key of ['file', 'find', 'fn']) assert.deepEqual(before[key], after[key])
    assert.equal(lineage.originalFailureUnchanged, true)
  }
  return catalogs
}

export function auditPrerequisite(manifest, run, jobs, artifacts) {
  const gate = manifest.prerequisite
  assert.equal(String(run.id), gate.runId)
  assert.equal(run.run_attempt, gate.attempt)
  assert.equal(run.head_sha, gate.definitionSha)
  assert.equal(run.head_branch, gate.definitionRef)
  assert.equal(run.status, 'completed')
  assert.equal(run.conclusion, 'success')
  for (const name of gate.requiredJobs) {
    const rows = jobs.filter((job) => job.name === name)
    assert.equal(rows.length, 1, 'Missing or duplicate prerequisite lane')
    assert.equal(rows[0].status, 'completed')
    assert.equal(rows[0].conclusion, 'success')
    assert.ok(rows[0].steps.length > 0)
    assert.ok(
      rows[0].steps.every((step) => step.conclusion === 'success' || step.conclusion === 'skipped'),
    )
  }
  assert.deepEqual(
    artifacts.map((artifact) => artifact.name).sort(),
    [...gate.requiredArtifacts].sort(),
  )
  for (const artifact of artifacts) {
    assert.equal(artifact.expired, false)
    assert.ok(artifact.size_in_bytes > 0 && Number.isSafeInteger(artifact.id))
    assert.match(artifact.digest, /^sha256:[a-f0-9]{64}$/)
    assert.equal(artifact.workflow_run.id, run.id)
    assert.equal(artifact.workflow_run.head_sha, run.head_sha)
  }
  return {
    scope: 'exact-product-three-lane-prerequisite-not-legacy-fault-gate',
    productSha: manifest.productSha,
    runId: String(run.id),
    attempt: run.run_attempt,
    definitionSha: run.head_sha,
    jobs: gate.requiredJobs,
  }
}

function auditJob(product, plan, job, identity, out, wasmSha256) {
  const catalogs = bindProduct(product, plan)
  const catalog = catalogs.get(job.catalog)
  const baseline = json(join(out, 'normal-baseline.json'))
  assert.deepEqual(baseline.identity, identity)
  assert.deepEqual(baseline.functions, job.baselineExpected)
  assert.deepEqual(baseline.ledger, job.ledgerRequirements)
  assert.deepEqual(baseline.sources, {
    productMainSha256: hash(checkedFile(product, 'tools/main-diff/main.ts')),
    ledgerFileSha256: hash(checkedFile(product, 'tools/main-diff/divergences.json')),
    mutatorSha256: plan.modules['tools/main-diff/faults.mjs'],
    wasmSha256,
  })
  const sources = Object.fromEntries(
    [...new Set(job.indices.map((index) => `crates/${catalog.faults[index].file}`))].map((path) => [
      path,
      checkedFile(product, path).toString('utf8'),
    ]),
  )
  const retained = loadRetainedEvidence(
    join(out, 'retained'),
    { indices: job.indices, faults: catalog.faults },
    baseline,
  )
  const receipt = auditRecordedRun({
    expected: {
      identity,
      modules: plan.modules,
      baselineSha256: canonicalMutationObjectHash(baseline),
      baselineIdentity: identity,
      baselineSources: baseline.sources,
      catalogs: [{ path: job.catalog, raw: catalog.raw }],
      shard: job.shard,
      shards: job.shards,
      cwd: join(product, 'tools/main-diff'),
      sources,
    },
    baseline,
    resultsRaw: readFileSync(join(out, 'faults.json')),
    ...retained,
  })
  const witness = auditWitness(job, retained)
  return {
    scope: plan.scope,
    identity,
    key: job.key,
    manifestSha256: plan.manifestSha256,
    planSha256: canonicalMutationObjectHash(plan),
    catalog: job.catalog,
    originalIndices: job.indices,
    receipt,
    typedWitness: witness,
    wasmSha256,
    baselineSha256: canonicalMutationObjectHash(baseline),
    fullRecoveryAccepted: false,
    currentProductReuseAccepted: false,
    originalVerdictsUnchanged: true,
  }
}

async function main(command, env) {
  assert.ok(['validate', 'prepare', 'check', 'baseline', 'audit', 'aggregate'].includes(command))
  const driver = resolve(env.DRIVER_DIR ?? 'driver'),
    product = resolve(env.PRODUCT_DIR ?? 'product')
  const manifest = json(join(driver, 'tools/main-diff/next-core-sensitivity-manifest.json'))
  const identity = nextIdentity(env, manifest),
    staticPlan = nextPlan(manifest, identity.definitionSha)
  assert.equal(git(driver, 'rev-parse', 'HEAD').toString().trim(), identity.definitionSha)
  if (command === 'validate') {
    const run = JSON.parse(
      execFileSync('gh', [
        'api',
        `repos/seuput-inc/seuoffice/actions/runs/${manifest.prerequisite.runId}`,
      ]),
    )
    const pages = JSON.parse(
      execFileSync('gh', [
        'api',
        '--paginate',
        '--slurp',
        `repos/seuput-inc/seuoffice/actions/runs/${manifest.prerequisite.runId}/jobs?per_page=100`,
      ]),
    )
    const artifactPages = JSON.parse(
      execFileSync('gh', [
        'api',
        '--paginate',
        '--slurp',
        `repos/seuput-inc/seuoffice/actions/runs/${manifest.prerequisite.runId}/artifacts?per_page=100`,
      ]),
    )
    const artifacts = artifactPages.flatMap((page) => page.artifacts)
    const prerequisite = auditPrerequisite(
      manifest,
      run,
      pages.flatMap((page) => page.jobs),
      artifacts,
    )
    save(resolve(env.PREREQUISITE_PATH), { prerequisite, run, jobs: pages, artifacts })
    emit(env, 'product_sha', identity.productSha)
    emit(
      env,
      'matrix',
      JSON.stringify({
        include: staticPlan.jobs.map((job) => ({
          key: job.key,
          catalog: job.catalog,
          shard: `${job.shard}/${job.shards}`,
        })),
      }),
    )
    return
  }
  const prerequisiteRaw = readFileSync(resolve(env.PREREQUISITE_PATH))
  const prerequisiteEvidence = JSON.parse(prerequisiteRaw)
  assert.deepEqual(
    prerequisiteEvidence.prerequisite,
    auditPrerequisite(
      manifest,
      prerequisiteEvidence.run,
      prerequisiteEvidence.jobs.flatMap((page) => page.jobs),
      prerequisiteEvidence.artifacts,
    ),
  )
  const plan = { ...staticPlan, prerequisiteEvidenceSha256: hash(prerequisiteRaw) }
  const catalogs = bindProduct(product, plan),
    planPath = resolve(env.PLAN_PATH)
  if (command === 'prepare') {
    save(planPath, plan)
    return
  }
  assert.deepEqual(json(planPath), plan)
  if (command === 'check') return
  if (command === 'aggregate') {
    const directory = resolve(env.COMPLETIONS_DIR)
    assert.ok(lstatSync(directory).isDirectory() && !lstatSync(directory).isSymbolicLink())
    assert.deepEqual(
      readdirSync(directory).sort(),
      plan.jobs.map((job) => `next-core-records-${job.key}`).sort(),
    )
    const completions = []
    for (const job of plan.jobs) {
      const out = join(directory, `next-core-records-${job.key}`)
      assert.ok(lstatSync(out).isDirectory() && !lstatSync(out).isSymbolicLink())
      const prior = json(join(out, 'completion.json'))
      assert.ok(digest(prior.wasmSha256))
      const actual = auditJob(product, plan, job, identity, out, prior.wasmSha256)
      assert.deepEqual(prior, actual)
      completions.push(actual)
    }
    assert.equal(new Set(completions.map((entry) => entry.key)).size, plan.jobs.length)
    save(resolve(env.AGGREGATE_PATH), {
      scope: plan.scope,
      identity,
      manifestSha256: plan.manifestSha256,
      planSha256: canonicalMutationObjectHash(plan),
      jobs: completions.length,
      records: completions.reduce((sum, entry) => sum + entry.receipt.records, 0),
      completions,
      fullRecoveryAccepted: false,
      currentProductReuseAccepted: false,
      originalVerdictsUnchanged: true,
    })
    return
  }
  assert.equal(env.MAIN_DIFF_PROFILE, 'node-faults')
  assert.equal(env.CARGO_INCREMENTAL, '1')
  assert.ok(isAbsolute(env.JOB_OUT) && isAbsolute(env.SEUOFFICE_WASM))
  const job = plan.jobs.find((entry) => entry.key === env.JOB_KEY)
  assert.ok(job, 'Unknown target job')
  const out = env.JOB_OUT
  mkdirSync(out, { recursive: true })
  if (command === 'baseline') {
    runSelectedBaseline(
      product,
      { ...job, modules: plan.modules },
      catalogs.get(job.catalog),
      identity,
      out,
      env,
    )
    return
  }
  assert.equal(command, 'audit')
  assert.equal(env.MUTATION_STATUS, 'success')
  assert.equal(resolve(env.MAIN_DIFF_MUTATION_EVIDENCE_DIR), join(out, 'retained'))
  const receipt = auditJob(
    product,
    plan,
    job,
    identity,
    out,
    hash(readFileSync(env.SEUOFFICE_WASM)),
  )
  save(join(out, 'completion.json'), receipt)
}

if (process.argv[1] && resolve(process.argv[1]) === self)
  main(process.argv[2], process.env).catch((error) => {
    console.error(error)
    process.exitCode = 1
  })
