import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'

export const mutationObjectHash = (value) =>
  createHash('sha256').update(JSON.stringify(value)).digest('hex')
const digest = (value) => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value)
const commit = (value) => typeof value === 'string' && /^[a-f0-9]{40}$/.test(value)
const catalogPath = /^tools\/main-diff\/faults\/[^/]+\.json$/
export const mutationKey = ({ catalog, index, objectSha256 }) => {
  assert.ok(catalogPath.test(catalog))
  assert.ok(Number.isSafeInteger(index) && index >= 0 && digest(objectSha256))
  return JSON.stringify([catalog, index, objectSha256])
}

/** Both catalogs are exact Git blobs. A position is not a mutation identity. */
export function indexMutationUniverses(originalProductSha, productSha, catalogs) {
  assert.ok(commit(originalProductSha) && commit(productSha))
  const entries = [],
    seen = new Set()
  for (const catalog of catalogs) {
    assert.ok(catalogPath.test(catalog.path) && !seen.has(catalog.path))
    seen.add(catalog.path)
    const original = JSON.parse(catalog.originalRaw)
    const current = JSON.parse(catalog.currentRaw)
    assert.ok(Array.isArray(original) && original.length > 0)
    assert.equal(current.length, original.length)
    for (let index = 0; index < original.length; index++) {
      for (const fault of [original[index], current[index]]) {
        assert.ok(typeof fault.file === 'string' && typeof fault.find === 'string')
        assert.ok(typeof fault.replace === 'string' && Array.isArray(fault.fn) && fault.fn.length)
        assert.ok(fault.fn.every((name) => typeof name === 'string' && name.length > 0))
      }
      const originalIdentity = {
        catalog: catalog.path,
        index,
        objectSha256: mutationObjectHash(original[index]),
      }
      const currentIdentity = {
        catalog: catalog.path,
        index,
        objectSha256: mutationObjectHash(current[index]),
      }
      entries.push({
        position: `${catalog.path}#${index}`,
        original: originalIdentity,
        current: currentIdentity,
        originalKey: mutationKey(originalIdentity),
        currentKey: mutationKey(currentIdentity),
        originalFault: original[index],
        currentFault: current[index],
      })
    }
  }
  assert.ok(entries.length > 0)
  return { originalProductSha, productSha, entries }
}

/** Inventory equality binds coverage and owner order, never absent case payloads. */
export function bindSelectedInventory({
  producerProductSha,
  productSha,
  selectors,
  before,
  after,
}) {
  assert.ok(commit(producerProductSha) && commit(productSha))
  assert.ok(Array.isArray(selectors) && selectors.length > 0)
  assert.ok(selectors.every((name) => typeof name === 'string' && name.length > 0))
  const wanted = new Set(selectors)
  assert.equal(wanted.size, selectors.length)
  const selected = (inventory) => {
    assert.ok(Array.isArray(inventory))
    const seen = new Set()
    const rows = []
    for (const row of inventory) {
      if (!wanted.has(row.name)) continue
      assert.ok(typeof row.group === 'string' && row.group.length > 0)
      assert.ok(Number.isSafeInteger(row.checks) && row.checks > 0)
      assert.equal(typeof row.single, 'boolean')
      const key = JSON.stringify([row.group, row.name])
      assert.ok(!seen.has(key), 'Duplicate function inventory entry')
      seen.add(key)
      rows.push({ group: row.group, name: row.name, checks: row.checks, single: row.single })
    }
    assert.deepEqual(
      [...new Set(rows.map((row) => row.name))].sort(),
      [...wanted].sort(),
      'Selected function missing from inventory',
    )
    return rows
  }
  const producerRows = selected(before)
  const currentRows = selected(after)
  assert.deepEqual(
    currentRows,
    producerRows,
    'Selected case count, single flag or name-owner registration order changed',
  )
  return {
    contract: 'selected-function-inventory-binding/v1',
    producerProductSha,
    productSha,
    rows: currentRows,
    selectedInventorySha256: mutationObjectHash(currentRows),
    casePayloadEqualityProven: false,
    historicalRuntimeCorpusCaptured: false,
  }
}

function receiptReference(reference) {
  assert.ok(reference && digest(reference.receiptSha256))
  assert.ok(commit(reference.definitionSha) && commit(reference.productSha))
  assert.match(reference.runId, /^[1-9][0-9]*$/)
  assert.ok(Number.isSafeInteger(reference.attempt) && reference.attempt > 0)
  assert.ok(Number.isSafeInteger(reference.jobId) && reference.jobId > 0)
  assert.ok(Number.isSafeInteger(reference.artifactId) && reference.artifactId > 0)
  assert.ok(digest(reference.archiveSha256))
  assert.equal(reference.jobStatus, 'completed')
  assert.equal(reference.jobConclusion, 'success')
  assert.ok(typeof reference.evidenceModel === 'string' && reference.evidenceModel.length > 0)
}

/**
 * Internal selection boundary, not a replacement native/raw-artifact auditor.
 * Callers supply facts only after exact archived native evidence is re-audited.
 * A semantic proof is an immutable, separately reviewed source certificate;
 * it never means that absent historical runtime inputs were reconstructed.
 */
export function selectVerifiedMutationFacts(universe, facts, semanticProofs, dispositions) {
  const byCurrent = new Map(universe.entries.map((entry) => [entry.currentKey, entry]))
  const proofs = new Map()
  for (const proof of semanticProofs) {
    assert.equal(proof.contract, 'reviewed-scoped-semantic-reuse/v1')
    assert.ok(digest(proof.proofSha256) && !proofs.has(proof.proofSha256))
    const body = { ...proof }
    delete body.proofSha256
    assert.equal(mutationObjectHash(body), proof.proofSha256, 'Semantic proof body changed')
    assert.ok(commit(proof.producerProductSha))
    assert.equal(proof.productSha, universe.productSha)
    assert.ok(Array.isArray(proof.selectors) && proof.selectors.length > 0)
    assert.equal(new Set(proof.selectors).size, proof.selectors.length)
    for (const field of [
      'sourceClosureSha256',
      'adapterBoundarySha256',
      'inputGenerationSha256',
      'normalizationSha256',
      'contextBoundarySha256',
      'ownerBindingSha256',
    ])
      assert.ok(digest(proof[field]), `Missing semantic proof boundary ${field}`)
    assert.equal(proof.historicalRuntimeCorpusClaimed, false)
    proofs.set(proof.proofSha256, proof)
  }
  const accepted = new Map()
  for (const fact of facts) {
    const key = mutationKey(fact.mutation),
      entry = byCurrent.get(key)
    assert.ok(entry, 'Native fact is not the current hash-qualified mutation')
    assert.equal(fact.verdict, 'CAUGHT')
    assert.equal(fact.completion, 'strict-complete-native')
    assert.deepEqual(fact.selectors, entry.currentFault.fn)
    receiptReference(fact.receipt)
    const sameProduct = fact.receipt.productSha === universe.productSha
    if (sameProduct) {
      assert.equal(fact.kind, 'direct-current-native')
    } else {
      assert.equal(fact.kind, 'historical-scoped-semantic-native')
      const proof = proofs.get(fact.semanticProofSha256)
      assert.ok(proof, 'Historical native fact has no reviewed semantic boundary')
      assert.equal(proof.producerProductSha, fact.receipt.productSha)
      assert.ok(
        entry.currentFault.fn.every((name) => proof.selectors.includes(name)),
        'Semantic proof omits a selected function',
      )
    }
    if (!accepted.has(key)) accepted.set(key, { mutation: entry.current, proofs: [] })
    accepted.get(key).proofs.push({
      kind: fact.kind,
      receipt: fact.receipt,
      ...(sameProduct ? {} : { semanticProofSha256: fact.semanticProofSha256 }),
    })
  }
  const changed = universe.entries.filter((entry) => entry.originalKey !== entry.currentKey)
  const classified = new Map()
  for (const disposition of dispositions) {
    const originalKey = mutationKey(disposition.original),
      currentKey = mutationKey(disposition.current)
    const entry = byCurrent.get(currentKey)
    assert.ok(entry && entry.originalKey === originalKey && originalKey !== currentKey)
    for (const field of ['file', 'find', 'fn'])
      assert.deepEqual(entry.originalFault[field], entry.currentFault[field])
    assert.ok(!classified.has(originalKey))
    assert.ok(
      [
        'nonbuilding-original',
        'invalid-positive-control',
        'equivalent-original-public-host',
      ].includes(disposition.kind),
    )
    assert.ok(['BUILD', 'CONTROL_PANIC', 'MISSED'].includes(disposition.originalVerdict))
    assert.equal(
      disposition.originalVerdict,
      disposition.kind === 'nonbuilding-original'
        ? 'BUILD'
        : disposition.kind === 'invalid-positive-control'
          ? 'CONTROL_PANIC'
          : 'MISSED',
    )
    if (disposition.kind === 'invalid-positive-control') assert.equal(entry.original.index, 0)
    if (disposition.kind === 'equivalent-original-public-host')
      assert.ok(disposition.assumptions?.includes('public-host-non-reentrant'))
    assert.equal(disposition.originalAccepted, false)
    assert.ok(digest(disposition.lineageProofSha256))
    classified.set(originalKey, disposition)
  }
  assert.deepEqual(
    [...classified.keys()].sort(),
    changed.map((entry) => entry.originalKey).sort(),
    'Every replacement needs an explicit original disposition',
  )
  const sameObject = universe.entries.filter((entry) => entry.originalKey === entry.currentKey)
  const acceptedOriginals = sameObject.filter((entry) => accepted.has(entry.currentKey))
  const required = universe.entries.filter((entry) => !accepted.has(entry.currentKey))
  return {
    contract: 'hash-qualified-current-reuse-selection/v1',
    productSha: universe.productSha,
    originalProductSha: universe.originalProductSha,
    currentUniverse: universe.entries.length,
    originalUniverse: universe.entries.length,
    acceptedCurrent: [...accepted.values()],
    acceptedCurrentCount: accepted.size,
    acceptedOriginals: acceptedOriginals.map((entry) => entry.original),
    acceptedOriginalCount: acceptedOriginals.length,
    acceptedReplacementCount: accepted.size - acceptedOriginals.length,
    originalDispositions: [...classified.values()],
    unresolvedSameOriginalObjects: sameObject
      .filter((entry) => !accepted.has(entry.currentKey))
      .map((entry) => entry.original),
    requiredCurrentEvidence: required.map((entry) => entry.current),
    requiredCurrentEvidenceCount: required.length,
    originalDispositionsAreNotSuccesses: true,
    historicalCorpusReconstructed: false,
    executionAuthorized: false,
    fullRecoveryAccepted: false,
    batch1Released: false,
  }
}

/** Planning data only: preserve the original runner's indices and control 0. */
export function planCurrentResidues(universe, selection) {
  assert.equal(selection.productSha, universe.productSha)
  const needed = new Set(selection.requiredCurrentEvidence.map(mutationKey))
  assert.equal(needed.size, selection.requiredCurrentEvidenceCount)
  const accepted = new Set(selection.acceptedCurrent.map((entry) => mutationKey(entry.mutation)))
  const known = new Set(universe.entries.map((entry) => entry.currentKey))
  assert.equal(accepted.size, selection.acceptedCurrentCount)
  assert.ok([...accepted, ...needed].every((key) => known.has(key)))
  assert.ok([...accepted].every((key) => !needed.has(key)))
  assert.deepEqual(
    [...new Set([...accepted, ...needed])].sort(),
    [...known].sort(),
    'Required and accepted mutations must partition the current universe',
  )
  const covered = new Set(),
    seenNoncontrols = new Set(),
    jobs = []
  const catalogs = [...new Set(universe.entries.map((entry) => entry.current.catalog))].sort()
  for (const catalog of catalogs) {
    const entries = universe.entries.filter((entry) => entry.current.catalog === catalog)
    assert.ok(entries.every((entry, index) => entry.current.index === index))
    const modulus = Math.max(4, 4 * Math.ceil((entries.length - 1) / 96))
    const residues = new Set(
      entries
        .filter((entry) => entry.current.index > 0 && needed.has(entry.currentKey))
        .map((entry) => entry.current.index % modulus),
    )
    if (needed.has(entries[0].currentKey) && residues.size === 0) residues.add(0)
    for (const residue of [...residues].sort((a, b) => a - b)) {
      const selected = entries.filter(
        (entry) => entry.current.index === 0 || entry.current.index % modulus === residue,
      )
      assert.ok(selected.length <= 25)
      for (const entry of selected) {
        covered.add(entry.currentKey)
        if (entry.current.index !== 0) {
          assert.equal(entry.current.index % 4, residue % 4)
          assert.ok(!seenNoncontrols.has(entry.currentKey))
          seenNoncontrols.add(entry.currentKey)
        }
      }
      jobs.push({
        catalog,
        shard: residue,
        shards: modulus,
        originalShard: residue % 4,
        mutations: selected.map((entry) => entry.current),
        required: selected
          .filter((entry) => needed.has(entry.currentKey))
          .map((entry) => entry.current),
      })
    }
  }
  assert.ok([...needed].every((key) => covered.has(key)))
  const batches = []
  for (let start = 0; start < jobs.length; start += 256)
    batches.push(jobs.slice(start, start + 256))
  return {
    productSha: universe.productSha,
    required: needed.size,
    expandedUnique: covered.size,
    records: jobs.reduce((count, job) => count + job.mutations.length, 0),
    jobs,
    batches,
    batchPolicy: 'sequential-after-strict-complete-aggregate',
    maxParallel: 8,
    maxRecords: 25,
    executionAuthorized: false,
    fullRecoveryAccepted: false,
    batch1Released: false,
  }
}
