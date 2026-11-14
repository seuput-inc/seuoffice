import assert from 'node:assert/strict'
import test from 'node:test'
import { hash, hashBytes } from './recovery-plan.mjs'
import { originalMutatorSha256 } from './recovery-runner-model.mjs'
import {
  validateManifest,
  makePlan,
  auditBaseline,
  auditTarget,
} from './missed-sensitivity-audit.mjs'

const h = (c) => c.repeat(64)
const wire = (value) => JSON.stringify({ answer: JSON.stringify({ value }), files: {} })
const reportHash = (report) => hashBytes(JSON.stringify(report, null, 2))

function syntheticUnit() {
  const control = { name: 'control', file: 'control.rs', find: 'a', replace: 'b', fn: ['control'] }
  const fault = { name: 'target', file: 'target.rs', find: 'c', replace: 'd', fn: ['target'] }
  const baselineExpected = [
    { group: 'unit', name: 'control', checks: 2, single: false },
    { group: 'unit', name: 'target', checks: 2, single: false },
  ]
  const manifest = {
    contract: 'main-missed-sensitivity-manifest/v1',
    productSha: 'a'.repeat(40),
    parentProductSha: '375b08bd0e1f7e8432e6514468cc9d25be01b100',
    originalRun: {
      runId: '43604410368',
      attempt: 1,
      definitionSha: '97ffeb5ec856c6a05399eae70525ede3568057ba',
      productSha: '375b08bd0e1f7e8432e6514468cc9d25be01b100',
    },
    mutatorSha256: originalMutatorSha256,
    scope: 'five-miss-followup-only-not-full-recovery',
    sourceLineage: {
      paths: [
        { path: 'tools/main-diff/groups/unit.ts', parentSha256: h('1'), productSha256: h('2') },
      ],
    },
    cases: [
      {
        id: 'unit',
        catalog: 'tools/main-diff/faults/unit.json',
        catalogSha256: h('3'),
        catalogLength: 5,
        index: 4,
        originalMutationSha256: hash(fault),
        mutationSha256: hash(fault),
        controlSha256: hash(control),
        fault,
        control,
        baselineExpected,
        caseRequirement: { fn: 'target', index: 1, note: 'new case', typescript: wire('expected') },
      },
    ],
  }
  const plan = makePlan(manifest, 'b'.repeat(40))
  const target = plan.cases[0]
  const identity = {
    definitionSha: plan.definitionSha,
    productSha: plan.productSha,
    manifestSha256: plan.manifestSha256,
    planSha256: hash(plan),
    runId: '123',
    attempt: 1,
    targetId: target.id,
  }
  const baseline = {
    identity,
    verdict: 'success',
    functions: baselineExpected,
    ledger: [],
    ledgerSha256: hash([]),
    sources: {
      productMainSha256: h('a'),
      ledgerFileSha256: h('b'),
      mutatorSha256: originalMutatorSha256,
      wasmSha256: h('d'),
    },
    reports: {
      unit: {
        report: {
          control: { checks: 2, equal: 2, listed: 0, distinct: 2 },
          target: { checks: 2, equal: 2, listed: 0, distinct: 2 },
        },
        differences: [],
      },
    },
    commands: [
      {
        group: 'unit',
        functions: ['control', 'target'],
        status: 0,
        signal: null,
        stdout:
          'control: 2 checks, 0 differences, 0 listed, 2 distinct\ntarget: 2 checks, 0 differences, 0 listed, 2 distinct\n',
        stderr: '',
      },
    ],
  }
  const report = {
    report: { target: { checks: 2, equal: 1, listed: 0, distinct: 2 } },
    differences: [
      {
        fn: 'target',
        case: 1,
        note: 'new case',
        typescript: wire('expected'),
        rust: wire('mutant'),
      },
    ],
  }
  const before = { 'crates/control.rs': h('e'), 'crates/target.rs': h('f') }
  const evidence = {
    identity,
    rows: [control, fault].map((f) => ({
      faults: target.catalog,
      name: f.name,
      file: f.file,
      fn: f.fn,
      verdict: 'CAUGHT',
      detail: `DIFF ${f.fn[0]} case 1`,
    })),
    text: '2 faults: 2 caught, 0 missed, 0 not building, 0 bad\n',
    report,
    reportSha256: reportHash(report),
    sourceBefore: before,
    sourceAfter: { ...before },
    sourceModel: {
      kind: 'pinned-original-runner-control-flow',
      mutatorSha256: originalMutatorSha256,
      entries: [
        { index: 0, path: 'crates/control.rs', findCount: 1, derivedAppliedSourceSha256: h('1') },
        { index: 4, path: 'crates/target.rs', findCount: 1, derivedAppliedSourceSha256: h('2') },
      ],
    },
  }
  return { manifest, plan, target, baseline, evidence }
}

function audit(f) {
  return auditTarget(f)
}
function refresh(f) {
  f.evidence.reportSha256 = reportHash(f.evidence.report)
}
function consumerUnit() {
  const f = syntheticUnit()
  const c = f.manifest.cases[0]
  const ledger = {
    id: 'fixed',
    fn: 'target',
    case: 0,
    typescript: JSON.parse(wire('fixed-ts')),
    rust: JSON.parse(wire('fixed-rust')),
  }
  const page = (totalChars) =>
    JSON.stringify({
      answer: JSON.stringify({
        value: [{ ok: true, name: 'whole.txt', text: '가😀', offset: 0, totalChars }],
        logs: [['read']],
      }),
      files: { 'whole.txt': '가😀' },
    })
  f.manifest.scope = 'ledger-consumer-followup-only-not-full-recovery'
  c.ledgerRequirements = [ledger]
  c.caseRequirement = {
    fn: 'target',
    index: 1,
    note: 'new case',
    typescript: page(3),
    valueChange: { path: ['value', 0, 'totalChars'], before: 3, after: 7 },
  }
  f.plan = makePlan(f.manifest, 'b'.repeat(40))
  f.target = f.plan.cases[0]
  f.baseline.identity.manifestSha256 = f.plan.manifestSha256
  f.baseline.identity.planSha256 = hash(f.plan)
  f.baseline.ledger = structuredClone([ledger])
  f.baseline.ledgerSha256 = hash(f.baseline.ledger)
  f.baseline.reports.unit.report.target = { checks: 2, equal: 1, listed: 1, distinct: 2 }
  f.baseline.commands[0].stdout = f.baseline.commands[0].stdout.replace(
    'target: 2 checks, 0 differences, 0 listed, 2 distinct',
    'target: 2 checks, 0 differences, 1 listed, 2 distinct',
  )
  f.evidence.report.report.target.equal = 0
  f.evidence.report.differences = [
    { fn: 'target', case: 0, typescript: JSON.stringify(ledger.typescript), rust: wire('changed') },
    { fn: 'target', case: 1, note: 'new case', typescript: page(3), rust: page(7) },
  ]
  f.evidence.rows[1].detail =
    'DIFF target case 0 | DIFF target case 1 | STALE ledger entry fixed: it no longer reproduces'
  refresh(f)
  return f
}

test('ledger consumer requires typed ledger and independent exact value change', () => {
  assert.equal(audit(consumerUnit()).typedNewCaseDifference.case, 1)
  for (const mutate of [
    (f) => {
      f.evidence.report.differences.shift()
      f.evidence.report.report.target.equal++
      f.evidence.rows[1].detail =
        'DIFF target case 1 | STALE ledger entry fixed: it no longer reproduces'
    },
    (f) => {
      f.evidence.report.differences.pop()
      f.evidence.report.report.target.equal++
      f.evidence.rows[1].detail =
        'DIFF target case 0 | STALE ledger entry fixed: it no longer reproduces'
    },
    (f) => {
      f.evidence.report.differences[0].typescript = wire('other')
    },
    (f) => {
      f.evidence.report.differences[0].rust = JSON.stringify(f.target.ledgerRequirements[0].rust)
    },
    (f) => {
      f.evidence.report.differences[1].typescript = wire('wrong')
    },
    (f) => {
      const d = f.evidence.report.differences[1]
      const w = JSON.parse(d.rust)
      const a = JSON.parse(w.answer)
      a.logs.push(['other'])
      w.answer = JSON.stringify(a)
      d.rust = JSON.stringify(w)
    },
    (f) => {
      const d = f.evidence.report.differences[1]
      const w = JSON.parse(d.rust)
      w.files.extra = 'bad'
      d.rust = JSON.stringify(w)
    },
    (f) => {
      const d = f.evidence.report.differences[1]
      const w = JSON.parse(d.rust)
      const a = JSON.parse(w.answer)
      a.error = 'bad'
      w.answer = JSON.stringify(a)
      d.rust = JSON.stringify(w)
    },
    (f) => {
      f.evidence.rows[1].detail += ' | STALE ledger entry unknown: it no longer reproduces'
    },
    (f) => {
      f.evidence.rows[1].detail += ' | STALE ledger entry fixed: it no longer reproduces'
    },
    (f) => {
      f.evidence.rows[1].detail = 'STALE ledger entry fixed: it no longer reproduces'
    },
    (f) => {
      f.baseline.commands[0].stdout += 'STALE ledger entry fixed: it no longer reproduces\n'
    },
    (f) => {
      f.baseline.ledger[0].typescript = JSON.parse(wire('wrong'))
      f.baseline.ledgerSha256 = hash(f.baseline.ledger)
    },
  ]) {
    const f = consumerUnit()
    mutate(f)
    refresh(f)
    assert.throws(() => audit(f))
  }
  const old = syntheticUnit()
  old.evidence.rows[1].detail += ' | STALE ledger entry fixed: it no longer reproduces'
  assert.throws(() => audit(old))
})

test('synthetic unit: bounded receipt requires control and appended typed case', () => {
  const f = syntheticUnit()
  assert.equal(validateManifest(f.manifest), true)
  assert.equal(f.target.modulus, 8)
  assert.equal(
    auditBaseline(
      { identity: f.baseline.identity, baselineExpected: f.target.baselineExpected },
      f.baseline,
    ).scope,
    'normal-baseline-only',
  )
  const receipt = audit(f)
  assert.equal(receipt.scope, 'missed-sensitivity-only')
  assert.equal(receipt.typedNewCaseDifference.typescript, wire('expected'))
  assert.equal(receipt.evidence.sourceModel.kind, 'pinned-original-runner-control-flow')
})

test('synthetic unit: explicit mutation replacement and added source lineage', () => {
  const f = syntheticUnit()
  const c = f.manifest.cases[0]
  const originalFault = structuredClone(c.fault)
  c.fault.name = 'replacement'
  c.fault.replace = 'new replacement'
  c.mutationSha256 = hash(c.fault)
  c.mutationLineage = {
    originalFault,
    originalSha256: hash(originalFault),
    followupSha256: c.mutationSha256,
    reason: 'replacement is non-equivalent',
  }
  f.manifest.sourceLineage.paths.push({
    path: 'docs/added.md',
    parentSha256: null,
    productSha256: h('4'),
  })
  assert.equal(validateManifest(f.manifest), true)
  for (const mutate of [
    (c) => {
      delete c.mutationLineage
    },
    (c) => {
      c.mutationLineage.originalSha256 = h('0')
    },
    (c) => {
      c.mutationLineage.followupSha256 = h('0')
    },
    (c) => {
      c.mutationLineage.reason = ''
    },
    (c) => {
      c.mutationLineage.originalFault.file = 'other.rs'
      c.mutationLineage.originalSha256 = hash(c.mutationLineage.originalFault)
      c.originalMutationSha256 = c.mutationLineage.originalSha256
    },
    (c) => {
      c.mutationLineage.originalFault.find = 'other'
      c.mutationLineage.originalSha256 = hash(c.mutationLineage.originalFault)
      c.originalMutationSha256 = c.mutationLineage.originalSha256
    },
    (c) => {
      c.mutationLineage.originalFault.fn = ['other']
      c.mutationLineage.originalSha256 = hash(c.mutationLineage.originalFault)
      c.originalMutationSha256 = c.mutationLineage.originalSha256
    },
  ]) {
    const invalid = structuredClone(f.manifest)
    mutate(invalid.cases[0])
    assert.throws(() => validateManifest(invalid))
  }
  const invalid = structuredClone(f.manifest)
  delete invalid.sourceLineage.paths[1].parentSha256
  assert.throws(() => validateManifest(invalid))
})

test('synthetic unit: manifest and identity boundaries fail closed', () => {
  for (const mutate of [
    (f) => {
      f.manifest.cases[0].index = 0
    },
    (f) => {
      f.manifest.cases[0].mutationSha256 = h('f')
    },
    (f) => {
      f.manifest.cases[0].controlSha256 = h('f')
    },
    (f) => {
      f.manifest.cases.push(structuredClone(f.manifest.cases[0]))
    },
    (f) => {
      f.manifest.sourceLineage.paths[0].productSha256 = 'bad'
    },
    (f) => {
      f.manifest.cases[0].caseRequirement.typescript = '{}'
    },
  ]) {
    const f = syntheticUnit()
    mutate(f)
    assert.throws(() => validateManifest(f.manifest))
  }
  for (const mutate of [
    (f) => {
      f.evidence.identity = { ...f.evidence.identity, targetId: 'other' }
    },
    (f) => {
      f.baseline.identity = { ...f.baseline.identity, planSha256: h('f') }
    },
    (f) => {
      f.plan.contract = 'main-fault-recovery/v1'
    },
    (f) => {
      f.baseline.commands[0].status = 1
    },
    (f) => {
      f.baseline.functions = f.baseline.functions.slice(1)
    },
    (f) => {
      f.baseline.commands[0].stdout += 'STALE ledger entry x: it no longer reproduces\n'
    },
    (f) => {
      f.baseline.commands[0].stderr = 'Error: bad\n'
    },
  ]) {
    const f = syntheticUnit()
    mutate(f)
    assert.throws(() => audit(f))
  }
})

test('synthetic unit: terminal evidence rejects wrong cases, sources, and diagnostics', () => {
  for (const mutate of [
    (f) => {
      f.evidence.rows.pop()
    },
    (f) => {
      f.evidence.rows[0].detail = 'DIFF other case 0'
    },
    (f) => {
      f.evidence.rows[1].detail += ' | FLOOR: fewer than two distinct answers'
    },
    (f) => {
      f.evidence.rows[1].detail += ' | STALE ledger entry x: it no longer reproduces'
    },
    (f) => {
      f.evidence.rows[1].detail += ' | Error: failed'
    },
    (f) => {
      f.evidence.text = '2 faults: 1 caught, 1 missed, 0 not building, 0 bad\n'
    },
    (f) => {
      f.evidence.sourceAfter['crates/target.rs'] = h('0')
    },
    (f) => {
      f.evidence.sourceModel.entries[1].findCount = 2
    },
    (f) => {
      f.evidence.sourceModel.entries[1].index = 3
    },
    (f) => {
      f.evidence.reportSha256 = h('0')
    },
    (f) => {
      f.evidence.report.report.other = f.evidence.report.report.target
      refresh(f)
    },
    (f) => {
      f.evidence.report.report.target.equal = 2
      refresh(f)
    },
    (f) => {
      f.evidence.report.differences[0].case = 0
      f.evidence.rows[1].detail = 'DIFF target case 0'
      refresh(f)
    },
    (f) => {
      f.evidence.report.differences[0].case = 2
      refresh(f)
    },
    (f) => {
      f.evidence.report.differences[0].typescript = wire('wrong')
      refresh(f)
    },
    (f) => {
      f.evidence.report.differences[0].rust = f.evidence.report.differences[0].typescript
      refresh(f)
    },
    (f) => {
      f.evidence.report.differences.push({ ...f.evidence.report.differences[0] })
      refresh(f)
    },
  ]) {
    const f = syntheticUnit()
    mutate(f)
    assert.throws(() => audit(f))
  }
})

const originalMisses = [
  [
    'tools/main-diff/faults/b5-mcp.json',
    'local a gone process is waited for',
    'main-core/src/shell/mcp_client/local.rs',
    'local.scenario',
    62,
  ],
  [
    'tools/main-diff/faults/b5-mcp.json',
    'runtimes a path is searched like a name',
    'main-core/src/shell/mcp_client/runtimes.rs',
    'runtimes.scenario',
    44,
  ],
  [
    'tools/main-diff/faults/b4-pdf-store.json',
    'migration replaces an equal provider record',
    'main-core/src/packages/pdf_structure/store.rs',
    'pdf-structure/store.migrateDocument',
    19,
  ],
  [
    'tools/main-diff/faults/b4-pdf-store.json',
    'insights need no three summaries',
    'main-core/src/packages/pdf_structure/reading.rs',
    'pdf-structure/reading.asInsights',
    14,
  ],
  [
    'tools/main-diff/faults/b4-pdf-store.json',
    'lock of a live process is stale',
    'main-core/src/packages/pdf_structure/files.rs',
    'pdf-structure/files.writeJsonAtomic',
    9,
  ],
]

test('real original five MISSED rows cannot be accepted as catches', () => {
  for (const [faults, name, file, fn, checks] of originalMisses) {
    const f = syntheticUnit()
    f.target.catalog = faults
    f.evidence.rows[0].faults = faults
    f.target.fault = { ...f.target.fault, name, file, fn: [fn] }
    f.target.mutationSha256 = hash(f.target.fault)
    f.baseline.identity.planSha256 = hash(f.plan)
    f.evidence.rows[1] = {
      faults,
      name,
      file,
      fn: [fn],
      verdict: 'MISSED',
      detail: `${checks} checks, ${checks} equal, 0 differences, 0 listed, 1 functions`,
    }
    assert.throws(() => audit(f), /Invalid mutation row/)
  }
})
