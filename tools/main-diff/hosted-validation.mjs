// The source prepush and baseline lanes share an exact product, not a legacy fault gate.
import assert from 'node:assert/strict'
import { execFileSync, spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { auditComparison } from './baseline-only.mjs'

const self = fileURLToPath(import.meta.url)
const sha = /^[0-9a-f]{40}$/
const hash = (value) => createHash('sha256').update(value).digest('hex')
export function canonicalObjectSha256(value) {
  return hash(JSON.stringify(value))
}
const read = (path) => JSON.parse(readFileSync(path, 'utf8'))

export function identity(env, manifest) {
  assert.equal(manifest.canonicalObjectHashContract, 'sha256-json-stringify-utf8/v1')
  assert.match(env.DEFINITION_SHA ?? '', sha, 'Invalid definition SHA')
  assert.equal(env.VALIDATION_PRODUCT_SHA, manifest.productSha, 'Wrong fixed validation product')
  assert.match(env.GITHUB_RUN_ID ?? '', /^[1-9][0-9]*$/, 'Invalid run ID')
  assert.match(env.GITHUB_RUN_ATTEMPT ?? '', /^[1-9][0-9]*$/, 'Invalid run attempt')
  assert.ok(['hosted-prepush', 'linux', 'windows'].includes(env.PROFILE), 'Invalid validation lane')
  for (const key of [
    'DOCX_FAULTS',
    'ODT_FAULTS',
    'MAIN_FAULTS',
    'FAULT_PRODUCT_SHA',
    'GATE_RUN_ID',
    'BASELINE_PRODUCT_SHA',
  ])
    assert.equal((env[key] ?? '').trim(), '', `${key} conflicts with validation`)
  assert.ok(['', 'false'].includes(env.ALL_FAULTS ?? ''), 'ALL_FAULTS conflicts with validation')
  assert.ok(['', '1'].includes(env.FAULT_SHARDS ?? ''), 'Validation has no fault shards')
  return {
    contract: manifest.contract,
    scope: manifest.scope,
    definitionSha: env.DEFINITION_SHA,
    productSha: manifest.productSha,
    baseSha: manifest.baseSha,
    runId: env.GITHUB_RUN_ID,
    attempt: Number(env.GITHUB_RUN_ATTEMPT),
    profile: env.PROFILE,
    manifestSha256: canonicalObjectSha256(manifest),
    fixedInventorySha256: canonicalObjectSha256(manifest.inventory),
    sourceRangeSha256: canonicalObjectSha256(manifest.sourceRange),
    reuseProposalSha256: manifest.reuseProposalSha256,
    receiptCorpusSha256: manifest.receiptCorpusSha256,
    canonicalObjectHashContract: 'sha256-json-stringify-utf8/v1',
    canonicalObjectHashDefinition: {
      sha: env.DEFINITION_SHA,
      path: 'tools/main-diff/hosted-validation.mjs',
      symbol: 'canonicalObjectSha256',
    },
    reuseAccepted: false,
    legacyFaultGateCompatible: false,
  }
}

export function validateReuseProposal(manifest, proposal, catalogs) {
  assert.equal(
    canonicalObjectSha256(proposal),
    manifest.reuseProposalSha256,
    'Wrong fixed reuse proposal',
  )
  assert.equal(proposal.productSha, manifest.productSha)
  assert.equal(proposal.currentProductReuseAccepted, false)
  assert.equal(proposal.dispatchAuthorized, false)
  assert.equal(proposal.oldFailuresUnchanged, true)
  const actual = new Map()
  assert.deepEqual([...catalogs.keys()].sort(), Object.keys(manifest.catalogSha256).sort())
  for (const [path, catalog] of catalogs) {
    assert.equal(manifest.catalogSha256[path], catalog.sha256, 'Wrong catalog bytes')
    for (const [index, fault] of catalog.faults.entries())
      actual.set(`${path}#${index}`, canonicalObjectSha256(fault))
  }
  const required = new Set(proposal.requiredIds)
  const reused = new Set(proposal.eligibleReuseCandidates.map((entry) => entry.id))
  assert.equal(required.size, proposal.requiredIds.length)
  assert.equal(reused.size, proposal.eligibleReuseCandidates.length)
  assert.ok(
    [...required].every((id) => !reused.has(id)),
    'Overlapping reuse and required IDs',
  )
  assert.deepEqual(
    [...actual.keys()].sort(),
    [...required, ...reused].sort(),
    'Incomplete ID universe',
  )
  for (const entry of proposal.eligibleReuseCandidates) {
    assert.equal(entry.mutationSha256, actual.get(entry.id), `Wrong candidate object ${entry.id}`)
  }
  for (const job of proposal.jobs) {
    assert.equal(job.indices.length, job.entryHashes.length)
    for (const [at, index] of job.indices.entries()) {
      assert.equal(job.entryHashes[at], actual.get(`${job.catalog}#${index}`))
    }
  }
}

export function bindReceiptCorpus(manifest, proposal, corpus) {
  assert.equal(canonicalObjectSha256(corpus), manifest.receiptCorpusSha256, 'Wrong receipt corpus')
  const strict = corpus.strictRecoverySnapshot
  assert.equal(canonicalObjectSha256(strict), proposal.sourceSnapshot.strictReceiptSha256)
  assert.equal(strict.fullRecoveryAccepted, false)
  const byHash = new Map(
    Object.entries(corpus)
      .filter(([name]) => name !== 'next24Manifest')
      .map(([, body]) => [canonicalObjectSha256(body), body]),
  )
  for (const candidate of proposal.eligibleReuseCandidates) {
    for (const reference of candidate.receipts) {
      if (reference.kind === 'strict-completed-job') {
        assert.equal(reference.productSha, strict.productSha)
        assert.equal(reference.runId, strict.runId)
        assert.ok(
          strict.accepted.some(
            (row) =>
              row.jobId === reference.jobId &&
              row.artifactId === reference.artifactId &&
              row.archiveSha256 === reference.archiveSha256 &&
              row.ids.includes(candidate.id),
          ),
          'Candidate is absent from the completed receipt',
        )
      } else {
        const body = byHash.get(reference.receiptSha256)
        assert.ok(body, 'Missing referenced receipt body')
        if (reference.kind === 'previous-reviewed-strict-unaffected-snapshot') {
          assert.equal(body.scope, 'fixed-original-completed-snapshot-only')
          assert.equal(body.original.productSha, reference.productSha)
          assert.equal(body.original.runId, reference.runId)
          const split = candidate.id.lastIndexOf('#')
          const catalog = candidate.id.slice(0, split),
            index = Number(candidate.id.slice(split + 1))
          assert.ok(
            body.completeEvidence.some(
              (row) => row.catalog === catalog && (index === 0 || index % 4 === row.shard),
            ),
            'Candidate is absent from the original shard',
          )
        } else {
          assert.equal(body.scope, reference.kind)
          assert.equal(body.productSha, reference.productSha)
          assert.equal(body.definitionSha, reference.definitionSha)
          assert.equal(body.runId, reference.runId)
          if (body.contract === 'next24-strict-raw-acceptance/v1') {
            const targetManifest = corpus.next24Manifest
            assert.equal(
              canonicalObjectSha256(targetManifest),
              body.manifestSha256,
              'Wrong bounded target receipt manifest',
            )
            assert.equal(targetManifest.productSha, body.productSha)
            assert.equal(body.allRequiredNativeProducerAndAuditStepsSucceeded, true)
            const rows = targetManifest.jobs.flatMap((job) =>
              job.entries
                .filter(
                  (entry) =>
                    `${job.catalog}#${entry.index}` === candidate.id &&
                    entry.mutationSha256 === candidate.mutationSha256,
                )
                .flatMap(() =>
                  body.bindings.filter(
                    (binding) =>
                      binding.key === job.key &&
                      binding.jobId === reference.jobId &&
                      binding.artifactId === reference.artifactId &&
                      binding.archiveSha256 === reference.archiveSha256 &&
                      binding.indices.some((index) => `${job.catalog}#${index}` === candidate.id),
                  ),
                ),
            )
            assert.equal(rows.length, 1, 'Candidate is absent from the bounded target receipt')
          }
        }
      }
    }
  }
  return {
    receiptBodies: byHash.size,
    candidateBindings: proposal.eligibleReuseCandidates.length,
    receiptCorpusSha256: manifest.receiptCorpusSha256,
    currentProductReuseAccepted: false,
  }
}

export function validateSourceObservation(manifest, observation) {
  assert.equal(observation.head, manifest.productSha, 'Wrong source checkout')
  assert.equal(observation.baseAncestor, manifest.baseSha, 'Wrong prepush merge base')
  assert.equal(observation.trackedChanges, '', 'Source changed before or during validation')
  assert.deepEqual(observation.range, manifest.sourceRange, 'Wrong complete source range')
  assert.equal(observation.prepushScriptSha256, manifest.prepushScriptSha256, 'Prepush changed')
  assert.equal(observation.registrySha256, manifest.registrySha256, 'Comparison registry changed')
}

export function assertPrepushSucceeded(result) {
  assert.equal(result.error, null, 'Prepush process could not run')
  assert.equal(result.signal, null, 'Prepush process was interrupted')
  assert.equal(result.status, 0, 'Hosted prepush failed')
}

export function auditBaseline(plan, manifest, inventory, reports, nativeReceipt) {
  assert.ok(['linux', 'windows'].includes(plan.profile))
  const expected =
    plan.profile === 'linux'
      ? { linux: manifest.inventory.linux }
      : {
          'windows-filtered': manifest.inventory['windows-filtered'],
          'windows-node-fs': manifest.inventory['windows-node-fs'],
        }
  assert.deepEqual(inventory, expected, 'Actual inventory differs from the fixed product inventory')
  assert.deepEqual(
    Object.keys(reports).sort(),
    Object.keys(expected).sort(),
    'Wrong baseline lanes',
  )
  const totals = {}
  for (const [lane, wanted] of Object.entries(expected)) {
    totals[lane] = auditComparison(
      wanted,
      lane === 'windows-node-fs' ? { 'node-fs': reports[lane] } : reports[lane],
    )
  }
  for (const key of ['definitionSha', 'productSha', 'runId', 'profile']) {
    assert.equal(nativeReceipt[key], plan[key], `Wrong baseline identity ${key}`)
  }
  assert.equal(Number(nativeReceipt.attempt), plan.attempt)
  assert.equal(nativeReceipt.contract, 'main-baseline-only/v1')
  assert.equal(nativeReceipt.comparisonVerdict, 'success')
  assert.equal(nativeReceipt.legacyFaultGateCompatible, false)
  assert.deepEqual(nativeReceipt.totals, totals)
  return totals
}

function git(dir, ...args) {
  return execFileSync('git', ['-C', dir, ...args], { encoding: 'utf8', maxBuffer: 64 << 20 }).trim()
}

function blob(dir, commit, path) {
  return execFileSync('git', ['-C', dir, 'show', `${commit}:${path}`], { maxBuffer: 64 << 20 })
}

function checkHeads(plan, source, driver) {
  assert.equal(git(driver, 'rev-parse', 'HEAD'), plan.definitionSha, 'Wrong driver checkout')
  assert.equal(git(driver, 'diff', '--name-only', 'HEAD'), '', 'Driver changed')
  assert.equal(
    git(driver, 'ls-files', '--others', '--exclude-standard'),
    '',
    'Untracked driver inputs',
  )
  assert.equal(git(source, 'rev-parse', 'HEAD'), plan.productSha, 'Wrong source checkout')
  assert.equal(git(source, 'diff', '--name-only', 'HEAD'), '', 'Tracked source changes')
  assert.equal(
    git(source, 'ls-files', '--others', '--exclude-standard'),
    '',
    'Untracked source inputs',
  )
}

function observeSource(plan, manifest, proposal, source, driver) {
  checkHeads(plan, source, driver)
  const changes = git(
    source,
    'diff',
    '--name-status',
    '--no-renames',
    `${plan.baseSha}...${plan.productSha}`,
  )
  const range = changes
    .split('\n')
    .filter(Boolean)
    .map((line) => {
      const [status, path] = line.split('\t')
      assert.ok(['A', 'M', 'D'].includes(status), 'Unexpected source change kind')
      return {
        path,
        beforeSha256: status === 'A' ? null : hash(blob(source, plan.baseSha, path)),
        afterSha256: status === 'D' ? null : hash(blob(source, plan.productSha, path)),
      }
    })
  const observation = {
    head: git(source, 'rev-parse', 'HEAD'),
    baseAncestor: git(source, 'merge-base', plan.baseSha, plan.productSha),
    trackedChanges: git(source, 'diff', '--name-only', 'HEAD'),
    range,
    prepushScriptSha256: hash(blob(source, plan.productSha, 'tools/prepush.mjs')),
    registrySha256: hash(blob(source, plan.productSha, 'tools/main-diff/groups/index.ts')),
  }
  validateSourceObservation(manifest, observation)
  const paths = new Set()
  const catalogs = new Map()
  const catalogPaths = [
    ...new Set(
      [...proposal.requiredIds, ...proposal.eligibleReuseCandidates.map((e) => e.id)].map((id) =>
        id.slice(0, id.lastIndexOf('#')),
      ),
    ),
  ].sort()
  for (const path of catalogPaths) {
    const raw = blob(source, plan.productSha, path)
    const faults = JSON.parse(raw)
    catalogs.set(path, { faults, sha256: hash(raw) })
    for (const fault of faults) paths.add(`crates/${fault.file}`)
  }
  validateReuseProposal(manifest, proposal, catalogs)
  assert.deepEqual(proposal.sourceBindings.map((entry) => entry.path).sort(), [...paths].sort())
  for (const entry of proposal.sourceBindings) {
    assert.equal(hash(blob(source, plan.productSha, entry.path)), entry.sha256)
    assert.equal(hash(blob(source, plan.baseSha, entry.path)), entry.sha256)
  }
  assert.equal(
    hash(blob(driver, plan.definitionSha, 'tools/main-diff/baseline-only.mjs')),
    manifest.baselineModuleSha256,
  )
  assert.equal(manifest.producerCanonicalObjectHash.productSha, plan.productSha)
  assert.equal(
    hash(blob(source, plan.productSha, manifest.producerCanonicalObjectHash.path)),
    manifest.producerCanonicalObjectHash.moduleSha256,
  )
  return {
    ...observation,
    sourceBindingsSha256: canonicalObjectSha256(proposal.sourceBindings),
    mutationSourceFiles: paths.size,
    mutationUniverse: proposal.universe,
    reuseProposalBound: true,
    reuseAccepted: false,
    sourceEvidenceModel:
      'exact Git blobs and clean tracked checkout; no per-compiler source observation claim',
  }
}

async function main(command, env) {
  const manifest = read(join(dirname(self), 'hosted-validation-manifest.json'))
  const proposal = read(join(dirname(self), manifest.reuseManifest))
  const corpus = read(join(dirname(self), manifest.receiptCorpus))
  const corpusBinding = bindReceiptCorpus(manifest, proposal, corpus)
  const driver = resolve(env.DRIVER_DIR ?? 'driver')
  const source = resolve(
    env.PROFILE === 'hosted-prepush'
      ? (env.SOURCE_DIR ?? 'source')
      : (env.PRODUCT_DIR ?? 'product'),
  )
  const planPath = resolve(env.VALIDATION_PLAN_PATH ?? 'validation-plan.json')
  const plan = identity(env, manifest)
  if (command === 'prepare') {
    assert.equal(git(driver, 'rev-parse', 'HEAD'), plan.definitionSha)
    writeFileSync(planPath, `${JSON.stringify(plan, null, 2)}\n`, { flag: 'wx' })
    if (env.GITHUB_OUTPUT) appendFileSync(env.GITHUB_OUTPUT, `product_sha=${plan.productSha}\n`)
    console.log(JSON.stringify(plan))
    return
  }
  assert.deepEqual(read(planPath), plan, 'Validation plan changed')
  const observation = observeSource(plan, manifest, proposal, source, driver)
  observation.receiptCorpusBinding = corpusBinding
  if (command === 'check') return
  const out = resolve(
    env.PROFILE === 'hosted-prepush'
      ? (env.VALIDATION_OUT ?? 'hosted-prepush')
      : join(source, 'tools/main-diff/out/validation'),
  )
  mkdirSync(out, { recursive: true })
  let result
  if (command === 'prepush') {
    assert.equal(plan.profile, 'hosted-prepush')
    assert.equal(
      hash(readFileSync(join(source, 'tools/prepush.mjs'))),
      manifest.prepushScriptSha256,
    )
    const args = ['tools/prepush.mjs', '--base', plan.baseSha]
    const run = spawnSync(process.execPath, args, { cwd: source, maxBuffer: 64 << 20 })
    const stdout = run.stdout ?? Buffer.alloc(0),
      stderr = run.stderr ?? Buffer.alloc(0)
    writeFileSync(join(out, 'stdout.txt'), stdout, { flag: 'wx' })
    writeFileSync(join(out, 'stderr.txt'), stderr, { flag: 'wx' })
    process.stdout.write(stdout)
    process.stderr.write(stderr)
    const producer = {
      command: process.execPath,
      args,
      cwd: source,
      status: run.status,
      signal: run.signal,
      error: run.error
        ? { name: run.error.name, message: run.error.message, code: run.error.code }
        : null,
      stdoutSha256: hash(stdout),
      stderrSha256: hash(stderr),
      buildEnvironment: {
        cargoIncremental: env.CARGO_INCREMENTAL ?? null,
        mainDiffProfile: env.MAIN_DIFF_PROFILE ?? null,
      },
    }
    writeFileSync(join(out, 'producer.json'), `${JSON.stringify(producer, null, 2)}\n`, {
      flag: 'wx',
    })
    assertPrepushSucceeded(producer)
    checkHeads(plan, source, driver)
    result = { ...plan, observation, prepushVerdict: 'success', producer }
  } else {
    assert.equal(command, 'baseline')
    const baselineOut = join(source, 'tools/main-diff/out')
    const inventory = read(join(baselineOut, 'inventory.json'))
    const nativeReceipt = read(join(baselineOut, 'identity.json'))
    const reports = Object.fromEntries(
      Object.keys(inventory).map((lane) => [lane, read(join(baselineOut, lane, 'report.json'))]),
    )
    const totals = auditBaseline(plan, manifest, inventory, reports, nativeReceipt)
    assert.equal(nativeReceipt.wasmSha256, hash(readFileSync(env.SEUOFFICE_WASM)))
    result = {
      ...plan,
      observation,
      comparisonVerdict: 'success',
      totals,
      wasmSha256: nativeReceipt.wasmSha256,
      requiresHostedPrepushInSameRun: true,
    }
  }
  writeFileSync(join(out, 'identity.json'), `${JSON.stringify(result, null, 2)}\n`, { flag: 'wx' })
  console.log(JSON.stringify(result, null, 2))
}

if (process.argv[1] && resolve(process.argv[1]) === self) {
  main(process.argv[2], process.env).catch((error) => {
    console.error(error)
    process.exitCode = 1
  })
}
