import assert from 'node:assert/strict'
import test from 'node:test'
import {
  indexMutationUniverses,
  mutationKey,
  mutationObjectHash,
  planCurrentResidues,
  selectVerifiedMutationFacts,
} from './current-reuse-selection.mjs'

const old = 'a'.repeat(40)
const current = 'b'.repeat(40)
const hash = 'c'.repeat(64)
const catalog = 'tools/main-diff/faults/example.json'
const fault = (index) => ({
  name: `fault ${index}`,
  file: 'src/example.rs',
  find: `old_${index}`,
  replace: `new_${index}`,
  fn: ['example.read', 'example.write'],
})
function universe(length = 2, replacement = false) {
  const original = Array.from({ length }, (_, i) => fault(i))
  const next = structuredClone(original)
  if (replacement) next[1].replace = 'corrected_1'
  return indexMutationUniverses(old, current, [
    { path: catalog, originalRaw: JSON.stringify(original), currentRaw: JSON.stringify(next) },
  ])
}
function reference(productSha = current) {
  return {
    receiptSha256: hash,
    definitionSha: old,
    productSha,
    runId: '123',
    attempt: 1,
    jobId: 2,
    artifactId: 3,
    archiveSha256: hash,
    jobStatus: 'completed',
    jobConclusion: 'success',
    evidenceModel: 'retained-source-restore/v1',
  }
}
function fact(entry, productSha = current) {
  return {
    mutation: entry.current,
    selectors: entry.currentFault.fn,
    verdict: 'CAUGHT',
    completion: 'strict-complete-native',
    receipt: reference(productSha),
    kind: productSha === current ? 'direct-current-native' : 'historical-scoped-semantic-native',
  }
}
function proof(selectors = fault(0).fn) {
  const body = {
    contract: 'reviewed-scoped-semantic-reuse/v1',
    producerProductSha: old,
    productSha: current,
    selectors,
    sourceClosureSha256: hash,
    adapterBoundarySha256: hash,
    inputGenerationSha256: hash,
    normalizationSha256: hash,
    contextBoundarySha256: hash,
    ownerBindingSha256: hash,
    historicalRuntimeCorpusClaimed: false,
  }
  return { ...body, proofSha256: mutationObjectHash(body) }
}
function disposition(entry) {
  return {
    original: entry.original,
    current: entry.current,
    kind: 'nonbuilding-original',
    originalVerdict: 'BUILD',
    originalAccepted: false,
    lineageProofSha256: hash,
  }
}

test('replacement success never counts as success of the original mutation', () => {
  const u = universe(2, true),
    entry = u.entries[1]
  const selected = selectVerifiedMutationFacts(u, [fact(entry)], [], [disposition(entry)])
  assert.equal(selected.acceptedCurrentCount, 1)
  assert.equal(selected.acceptedOriginalCount, 0)
  assert.equal(selected.acceptedReplacementCount, 1)
  assert.equal(selected.originalDispositions[0].originalVerdict, 'BUILD')
  assert.notEqual(mutationKey(entry.original), mutationKey(entry.current))
  assert.throws(() =>
    selectVerifiedMutationFacts(
      u,
      [{ ...fact(entry), mutation: entry.original }],
      [],
      [disposition(entry)],
    ),
  )
  assert.throws(() => selectVerifiedMutationFacts(u, [], [], []))
})

test('partial, failed and differently selected native evidence cannot subtract a mutation', () => {
  const u = universe(),
    valid = fact(u.entries[1])
  for (const patch of [
    { verdict: 'MISSED' },
    { completion: 'prefix-only' },
    { selectors: ['example.read'] },
    { receipt: { ...valid.receipt, jobConclusion: 'failure' } },
    { receipt: { ...valid.receipt, archiveSha256: 'missing' } },
  ])
    assert.throws(() => selectVerifiedMutationFacts(u, [{ ...valid, ...patch }], [], []))
})

test('historical subtraction requires all selectors and an unchanged proof body', () => {
  const u = universe(),
    entry = u.entries[1],
    p = proof()
  const historical = { ...fact(entry, old), semanticProofSha256: p.proofSha256 }
  const selected = selectVerifiedMutationFacts(u, [historical], [p], [])
  assert.equal(selected.acceptedOriginalCount, 1)
  assert.throws(() => selectVerifiedMutationFacts(u, [historical], [], []))
  const incomplete = proof(['example.read'])
  assert.throws(() =>
    selectVerifiedMutationFacts(
      u,
      [{ ...historical, semanticProofSha256: incomplete.proofSha256 }],
      [incomplete],
      [],
    ),
  )
  assert.throws(() =>
    selectVerifiedMutationFacts(
      u,
      [historical],
      [{ ...p, ownerBindingSha256: 'd'.repeat(64) }],
      [],
    ),
  )
})

test('unknown corpus without a source certificate remains required rather than inferred', () => {
  const u = universe(4)
  const selected = selectVerifiedMutationFacts(u, [fact(u.entries[1])], [], [])
  assert.deepEqual(
    selected.requiredCurrentEvidence.map((m) => m.index),
    [0, 2, 3],
  )
  assert.equal(selected.historicalCorpusReconstructed, false)
  assert.equal(selected.executionAuthorized, false)
})

test('equivalent public-host replacement requires its non-reentrant premise', () => {
  const u = universe(2, true),
    entry = u.entries[1]
  const equivalent = {
    ...disposition(entry),
    kind: 'equivalent-original-public-host',
    originalVerdict: 'MISSED',
    assumptions: ['public-host-non-reentrant'],
  }
  assert.equal(selectVerifiedMutationFacts(u, [], [], [equivalent]).originalDispositions.length, 1)
  assert.throws(() => selectVerifiedMutationFacts(u, [], [], [{ ...equivalent, assumptions: [] }]))
})

test('residue plans cover required hashes without duplicate noncontrols or index renumbering', () => {
  const u = universe(1237)
  const selected = selectVerifiedMutationFacts(u, [fact(u.entries[3])], [], [])
  const plan = planCurrentResidues(u, selected)
  const seen = new Set()
  for (const job of plan.jobs) {
    assert.ok(job.mutations.length <= 25)
    assert.equal(job.mutations[0].index, 0)
    for (const m of job.mutations.filter((m) => m.index !== 0)) {
      assert.equal(m.index % job.shards, job.shard)
      assert.equal(m.index % 4, job.originalShard)
      assert.ok(!seen.has(mutationKey(m)))
      seen.add(mutationKey(m))
    }
  }
  for (const m of selected.requiredCurrentEvidence.filter((m) => m.index !== 0))
    assert.ok(seen.has(mutationKey(m)))
  assert.ok(plan.batches.every((batch) => batch.length <= 256))
  assert.equal(plan.maxParallel, 8)
  assert.equal(plan.executionAuthorized, false)
  assert.equal(plan.batch1Released, false)
})

test('a forged accepted/required partition cannot silently omit a mutation', () => {
  const u = universe(4),
    selected = selectVerifiedMutationFacts(u, [], [], [])
  selected.requiredCurrentEvidence.pop()
  selected.requiredCurrentEvidenceCount--
  assert.throws(() => planCurrentResidues(u, selected))
})
