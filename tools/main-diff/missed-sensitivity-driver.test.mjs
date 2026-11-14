import assert from 'node:assert/strict'
import test from 'node:test'
import { resolve } from 'node:path'
import {
  assertSelectedLedger,
  bindCatalogCase,
  identity,
  terminalReportPath,
  validateLineage,
} from './missed-sensitivity-driver.mjs'
import { hash } from './recovery-plan.mjs'

const sha = 'a'.repeat(40)
const manifest = {
  productSha: sha,
  scope: 'five-miss-followup-only-not-full-recovery',
  cases: [{ id: 'target' }],
}
const env = {
  MODE: 'main-missed-sensitivity',
  PRODUCT_SHA: sha,
  DEFINITION_SHA: 'b'.repeat(40),
  GITHUB_RUN_ID: '123',
  GITHUB_RUN_ATTEMPT: '1',
  TARGET_ID: 'target',
}

test('identity binds exact product, definition, run and target', () => {
  assert.deepEqual(identity(env, manifest, true), {
    productSha: sha,
    definitionSha: 'b'.repeat(40),
    runId: '123',
    attempt: 1,
  })
  for (const change of [
    { MODE: 'wrong' },
    { PRODUCT_SHA: 'c'.repeat(40) },
    { DEFINITION_SHA: 'b'.repeat(39) },
    { GITHUB_RUN_ID: '0' },
    { GITHUB_RUN_ATTEMPT: '0' },
    { TARGET_ID: '' },
    { TARGET_ID: 'unknown' },
  ])
    assert.throws(() => identity({ ...env, ...change }, manifest, true))
})

test('identity binds each explicit mode to its manifest scope', () => {
  const modes = [
    ['five-miss-followup-only-not-full-recovery', 'main-missed-sensitivity'],
    ['ledger-consumer-followup-only-not-full-recovery', 'main-ledger-consumer-sensitivity'],
    ['ledger-consumer-suite-only-not-full-recovery', 'main-ledger-consumer-suite'],
  ]
  for (const [scope, mode] of modes) {
    const selectedManifest = { ...manifest, scope }
    assert.deepEqual(
      identity({ ...env, MODE: mode }, selectedManifest, true),
      identity(env, manifest, true),
    )
    for (const [, otherMode] of modes)
      if (otherMode !== mode)
        assert.throws(() => identity({ ...env, MODE: otherMode }, selectedManifest, true))
  }
  assert.throws(() => identity(env, { ...manifest, scope: 'unknown' }, true))
})

test('selected ledger binds full declared entries and defaults to empty', () => {
  const entry = { id: 'boundary', fn: 'readAttachmentPage', ts: { value: 3 }, rust: { value: 4 } }
  assertSelectedLedger([entry], { ledgerRequirements: [entry] })
  assertSelectedLedger([], {})
  assert.throws(() => assertSelectedLedger([entry], {}))
  assert.throws(() => assertSelectedLedger([], { ledgerRequirements: [entry] }))
  assert.throws(() =>
    assertSelectedLedger([{ ...entry, rust: { value: 5 } }], {
      ledgerRequirements: [entry],
    }),
  )
})

test('terminal report resolves against the product runner directory', () => {
  assert.equal(
    terminalReportPath('/product', { MAIN_DIFF_OUT: 'relative' }),
    resolve('/product/tools/main-diff/relative/report.json'),
  )
  assert.equal(
    terminalReportPath('/product', { MAIN_DIFF_OUT: '/job/mutant' }),
    '/job/mutant/report.json',
  )
  assert.throws(() => terminalReportPath('/product', {}))
})

test('lineage accepts only the exact reviewed paths and hashes', () => {
  const item = { path: 'a', parentSha256: 'one', productSha256: 'two' }
  const lineage = { sourceLineage: { paths: [item] } }
  const hashes = { a: { parentSha256: 'one', productSha256: 'two' } }
  validateLineage(lineage, ['a'], hashes)
  assert.throws(() => validateLineage(lineage, ['a', 'b'], hashes))
  assert.throws(() =>
    validateLineage(lineage, ['a'], { a: { parentSha256: 'one', productSha256: 'wrong' } }),
  )
})

test('catalog identity binds declared replacement to the original entry', () => {
  const control = { name: 'control', file: 'x', fn: ['a'], find: 'old', replace: 'new' }
  const original = { name: 'original', file: 'x', fn: ['b'], find: 'before', replace: 'after' }
  const followup = { ...original, name: 'replacement', replace: 'other' }
  const item = {
    index: 1,
    catalogLength: 2,
    originalMutationSha256: hash(original),
    mutationSha256: hash(followup),
    controlSha256: hash(control),
    control,
    fault: followup,
    mutationLineage: {
      originalFault: original,
      originalSha256: hash(original),
      followupSha256: hash(followup),
      reason: 'explicit replacement',
    },
  }
  bindCatalogCase(item, [control, original], [control, followup])
  assert.throws(() =>
    bindCatalogCase(item, [control, { ...original, find: 'other' }], [control, followup]),
  )
  assert.throws(() =>
    bindCatalogCase(
      { ...item, mutationLineage: undefined },
      [control, original],
      [control, followup],
    ),
  )
  assert.throws(() => bindCatalogCase(item, [control, original, control], [control, followup]))
})
