import { createHash } from 'node:crypto'
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { auditRecordedRun, auditRetainedBaseline } from './mutation-evidence-audit.mjs'

const hash = (value) => createHash('sha256').update(value).digest('hex')
const json = (value) => `${JSON.stringify(value, null, 2)}\n`
const objectHash = (value) => hash(JSON.stringify(value))

function fixture() {
  const identity = {
    productSha: 'a'.repeat(40),
    definitionSha: 'b'.repeat(40),
    runId: '42',
    attempt: 1,
  }
  const modules = {
    'tools/main-diff/faults.mjs': 'c'.repeat(64),
    'tools/main-diff/mutation-evidence.mjs': 'd'.repeat(64),
  }
  const sources = {
    productMainSha256: 'e'.repeat(64),
    ledgerFileSha256: 'f'.repeat(64),
    mutatorSha256: modules['tools/main-diff/faults.mjs'],
    wasmSha256: '0'.repeat(64),
  }
  const ledger = [
    {
      id: 'fixed-ledger',
      fn: 'example.read',
      case: 0,
      typescript: { answer: '{"value":"typescript"}', files: {} },
      rust: { answer: '{"value":"fixed rust"}', files: {} },
    },
  ]
  const baseline = {
    identity,
    functions: [{ name: 'example.read', group: 'example', checks: 3, single: false }],
    ledger,
    ledgerSha256: objectHash(ledger),
    reports: {
      example: {
        report: {
          'example.read': { checks: 3, equal: 2, listed: 1, distinct: 3 },
        },
        differences: [],
      },
    },
    commands: [
      {
        group: 'example',
        functions: ['example.read'],
        status: 0,
        signal: null,
        stdout: 'example.read: 3 checks, 0 differences, 1 listed, 3 distinct\n',
        stderr: '',
      },
    ],
    sources,
    verdict: 'success',
  }
  const fault = {
    name: 'control',
    file: 'example.rs',
    find: 'before',
    replace: 'after',
    fn: ['example.read'],
  }
  const catalogRaw = json([fault])
  const catalog = {
    ordinal: 0,
    path: 'catalog.json',
    sha256: hash(catalogRaw),
    length: 1,
  }
  const expected = {
    identity,
    modules,
    baselineSha256: objectHash(baseline),
    baselineIdentity: identity,
    baselineSources: sources,
    catalogs: [{ path: catalog.path, raw: catalogRaw }],
    shard: 0,
    shards: 1,
    cwd: '/product/tools/main-diff',
    sources: { 'crates/example.rs': 'before' },
  }
  const run = {
    contract: 'per-mutation-producer-evidence/v1',
    identity,
    producer: { modules },
    buildEnvironment: { profile: 'node-faults', cargoIncremental: '1' },
    catalogPaths: [catalog.path],
    shard: 0,
    shards: 1,
  }
  const start = {
    catalog,
    index: 0,
    mutationSha256: objectHash(fault),
    fault,
    isControl: true,
    source: {
      path: 'crates/example.rs',
      exists: true,
      beforeSha256: hash('before'),
      findCount: 1,
      derivedAppliedSourceSha256: hash('after'),
    },
  }
  const applied = { path: start.source.path, observedSha256: hash('after') }
  const report = {
    report: { 'example.read': { checks: 3, equal: 1, listed: 0, distinct: 3 } },
    differences: [
      {
        fn: 'example.read',
        case: 0,
        typescript: JSON.stringify(ledger[0].typescript),
        rust: JSON.stringify({ answer: '{"value":"changed rust"}', files: {} }),
      },
      {
        fn: 'example.read',
        case: 2,
        typescript: JSON.stringify({
          answer:
            '{"value":[{"ok":true,"name":"whole.txt","totalChars":3,"offset":0,"text":"가😀"}],"logs":[["diagramPictureExtOf","$DIR/whole.txt"],["assertFileSaved","$DIR/whole.txt"],["parseFileToText","$DIR/whole.txt"]]}',
          files: { 'whole.txt': 'file 644 "가😀"' },
        }),
        rust: JSON.stringify({
          answer:
            '{"value":[{"ok":true,"name":"whole.txt","totalChars":7,"offset":0,"text":"가😀"}],"logs":[["diagramPictureExtOf","$DIR/whole.txt"],["assertFileSaved","$DIR/whole.txt"],["parseFileToText","$DIR/whole.txt"]]}',
          files: { 'whole.txt': 'file 644 "가😀"' },
        }),
      },
    ],
  }
  const command = {
    position: 0,
    group: 'example',
    functions: ['example.read'],
    command: 'npx',
    args: ['tsx', 'main.ts', '--group', 'example', '--show', '3', '--fn', 'example.read'],
    cwd: expected.cwd,
    status: 1,
    signal: null,
    stdout:
      'example.read: 3 checks, 2 differences, 0 listed, 3 distinct\n\nDIFF example.read case 0\n  typescript: "RuntimeArchiveError: data"\nDIFF example.read case 2\nSTALE ledger entry fixed-ledger: it no longer reproduces\n',
    stderr: '',
    reportSha256: '',
    sourceSha256AtCompare: hash('after'),
  }
  return {
    expected,
    baseline,
    run,
    catalog,
    start,
    applied,
    report,
    command,
    endOverride: {},
    finishedOverride: {},
  }
}

function packageEvidence(f) {
  const runRaw = json(f.run)
  const startRaw = json(f.start)
  const appliedRaw = json(f.applied)
  const reportRaw = json(f.report)
  const commandRaw = json({ ...f.command, reportSha256: hash(reportRaw) })
  const detail = f.command.stdout
    .concat(f.command.stderr)
    .split('\n')
    .filter(
      (line) =>
        line.startsWith('DIFF ') ||
        / checks, \d+ equal/.test(line) ||
        /FLOOR|STALE|Error/.test(line),
    )
    .slice(0, 4)
    .join(' | ')
  const end = {
    startSha256: hash(startRaw),
    appliedSha256: hash(appliedRaw),
    groups: [
      {
        position: 0,
        commandSha256: hash(commandRaw),
        reportSha256: hash(reportRaw),
      },
    ],
    verdict: 'CAUGHT',
    detail,
    sourceAfter: { exists: true, sha256: hash('before') },
    restored: true,
    ...f.endOverride,
  }
  const endRaw = json(end)
  const rows = [
    {
      faults: f.catalog.path,
      name: f.start.fault.name,
      file: f.start.fault.file,
      fn: f.start.fault.fn,
      verdict: end.verdict,
      detail: end.detail,
    },
  ]
  const resultsRaw = json(rows)
  const finished = {
    runSha256: hash(runRaw),
    catalogs: [{ ordinal: 0, sha256: f.catalog.sha256 }],
    records: [
      {
        catalogOrdinal: 0,
        index: 0,
        startSha256: hash(startRaw),
        endSha256: hash(endRaw),
      },
    ],
    resultsSha256: hash(resultsRaw),
    finalBuildCompleted: true,
    ...f.finishedOverride,
  }
  return {
    expected: f.expected,
    baseline: f.baseline,
    runRaw,
    catalogsRaw: [json(f.catalog)],
    records: [
      {
        catalogOrdinal: 0,
        index: 0,
        startRaw,
        appliedRaw,
        groups: [{ commandRaw, reportRaw }],
        endRaw,
      },
    ],
    resultsRaw,
    finishedRaw: json(finished),
  }
}

function rejects(change) {
  const f = fixture()
  change(f)
  assert.throws(() => auditRecordedRun(packageEvidence(f)))
}

test('accepts bound typed independent difference and changed stale tuple', () => {
  assert.deepEqual(auditRecordedRun(packageEvidence(fixture())), {
    scope: 'per-mutation-producer-evidence/v1',
    records: 1,
  })
})

test('rejects catalog byte length mislabeled as entry count', () => {
  rejects((f) => {
    f.catalog.length = Buffer.byteLength(f.expected.catalogs[0].raw)
  })
})

test('rejects changed tuple, absent tuple and only-stale evidence', () => {
  rejects((f) => {
    f.report.differences[0].rust = JSON.stringify(f.baseline.ledger[0].rust)
  })
  rejects((f) => {
    f.report.differences.shift()
    f.report.report['example.read'].equal++
  })
  rejects((f) => {
    f.report.differences.pop()
    f.report.report['example.read'].equal++
    f.command.stdout = f.command.stdout
      .replace('2 differences', '1 differences')
      .replace('DIFF example.read case 2\n', '')
  })
})

test('rejects invalid typed wire structure on each mutation and fixed ledger side', () => {
  const invalid = [
    1,
    [],
    { oops: 1 },
    { answer: '{}' },
    { files: {} },
    { answer: 'not json', files: {} },
    { answer: '[]', files: {} },
    { answer: 'null', files: {} },
    { answer: '{}', files: [] },
  ]
  for (const value of invalid) {
    for (const side of ['typescript', 'rust']) {
      rejects((f) => {
        f.report.differences[1][side] = JSON.stringify(value)
      })
      rejects((f) => {
        f.baseline.ledger[0][side] = value
        f.baseline.ledgerSha256 = objectHash(f.baseline.ledger)
        f.expected.baselineSha256 = objectHash(f.baseline)
        if (side === 'typescript') f.report.differences[0].typescript = JSON.stringify(value)
      })
    }
  }
})

test('rejects non-string file values on mutation and fixed ledger sides', () => {
  for (const side of ['typescript', 'rust']) {
    const mutation = fixture()
    const wire = JSON.parse(mutation.report.differences[1][side])
    wire.files['bad.txt'] = 42
    mutation.report.differences[1][side] = JSON.stringify(wire)
    assert.throws(() => auditRecordedRun(packageEvidence(mutation)), /Invalid typed wire/)

    const fixed = fixture()
    fixed.baseline.ledger[0][side].files['bad.txt'] = 42
    fixed.baseline.ledgerSha256 = objectHash(fixed.baseline.ledger)
    fixed.expected.baselineSha256 = objectHash(fixed.baseline)
    assert.throws(() => auditRecordedRun(packageEvidence(fixed)), /Invalid typed wire/)
  }
})

test('rejects baseline diagnostics even when the modified baseline is pinned', () => {
  for (const stream of ['stdout', 'stderr']) {
    for (const diagnostic of [
      'unknown selector: x',
      'unknown group: x',
      'unknown function: x',
      'fatal: comparison failed',
    ]) {
      const f = fixture()
      f.baseline.commands[0][stream] += `${diagnostic}\n`
      f.expected.baselineSha256 = objectHash(f.baseline)
      assert.throws(() => auditRetainedBaseline(f.baseline), /Fatal comparison diagnostic/)
      assert.throws(() => auditRecordedRun(packageEvidence(f)), /Fatal comparison diagnostic/)
    }
  }
})

test('rejects malformed, duplicate, unknown and fatal diagnostics', () => {
  for (const line of [
    'STALE ledger entry unknown: it no longer reproduces',
    'STALE broken',
    'DIFF example.read case x',
    'RuntimeArchiveError: fatal',
    'no such group: example',
    'FLOOR: fewer than two distinct answers',
    'DIFF example.read case 0',
  ]) {
    rejects((f) => {
      f.command.stderr = `${line}\n`
    })
  }
})

test('rejects identity, source, command, restoration and finish failures', () => {
  rejects((f) => {
    f.run.producer.modules = {
      ...f.run.producer.modules,
      'tools/main-diff/faults.mjs': '9'.repeat(64),
    }
  })
  rejects((f) => {
    f.run.buildEnvironment.profile = 'release'
  })
  rejects((f) => {
    f.start.source.derivedAppliedSourceSha256 = hash('wrong')
  })
  rejects((f) => {
    f.command.args.reverse()
  })

  rejects((f) => {
    f.command.sourceSha256AtCompare = hash('before')
  })
  rejects((f) => {
    f.endOverride.restored = false
  })
  rejects((f) => {
    f.endOverride.verdict = 'MISSED'
  })
  rejects((f) => {
    f.finishedOverride.finalBuildCompleted = false
  })
})
test('requires every original selected index in order', () => {
  const first = fixture()
  const second = fixture()
  const nextFault = { ...first.start.fault, name: 'second' }
  const raw = json([first.start.fault, nextFault])
  const catalog = { ...first.catalog, sha256: hash(raw), length: 2 }
  for (const f of [first, second]) {
    f.catalog = catalog
    f.start.catalog = catalog
    f.expected.catalogs = [{ path: catalog.path, raw }]
  }
  second.start.index = 1
  second.start.isControl = false
  second.start.fault = nextFault
  second.start.mutationSha256 = objectHash(nextFault)
  const p = packageEvidence(first)
  const q = packageEvidence(second)
  q.records[0].index = 1
  p.records.push(q.records[0])
  p.resultsRaw = json([...JSON.parse(p.resultsRaw), ...JSON.parse(q.resultsRaw)])
  const finished = JSON.parse(p.finishedRaw)
  finished.records.push({
    catalogOrdinal: 0,
    index: 1,
    startSha256: hash(q.records[0].startRaw),
    endSha256: hash(q.records[0].endRaw),
  })
  finished.resultsSha256 = hash(p.resultsRaw)
  p.finishedRaw = json(finished)
  assert.equal(auditRecordedRun(p).records, 2)
  assert.throws(() => auditRecordedRun({ ...p, records: [p.records[1], p.records[0]] }))
  assert.throws(() => auditRecordedRun({ ...p, records: [p.records[1]] }))
})

test('requires every selected group and its original argv order', () => {
  const f = fixture()
  f.baseline.functions.push({
    name: 'second.read',
    group: 'second',
    checks: 2,
    single: false,
  })
  f.baseline.reports.second = {
    report: { 'second.read': { checks: 2, equal: 2, listed: 0, distinct: 2 } },
    differences: [],
  }
  f.baseline.commands.push({
    group: 'second',
    functions: ['second.read'],
    status: 0,
    signal: null,
    stdout: 'second.read: 2 checks, 0 differences, 0 listed, 2 distinct\n',
    stderr: '',
  })
  f.expected.baselineSha256 = objectHash(f.baseline)
  f.start.fault.fn.push('second.read')
  f.start.mutationSha256 = objectHash(f.start.fault)
  const raw = json([f.start.fault])
  f.catalog.sha256 = hash(raw)
  f.catalog.length = 1
  f.expected.catalogs[0].raw = raw
  const p = packageEvidence(f)
  const reportRaw = json(f.baseline.reports.second)
  const commandRaw = json({
    position: 1,
    group: 'second',
    functions: ['second.read'],
    command: 'npx',
    args: ['tsx', 'main.ts', '--group', 'second', '--show', '3', '--fn', 'second.read'],
    cwd: f.expected.cwd,
    status: 0,
    signal: null,
    stdout: f.baseline.commands[1].stdout,
    stderr: '',
    reportSha256: hash(reportRaw),
    sourceSha256AtCompare: hash('after'),
  })
  p.records[0].groups.push({ commandRaw, reportRaw })
  const end = JSON.parse(p.records[0].endRaw)
  end.groups.push({
    position: 1,
    commandSha256: hash(commandRaw),
    reportSha256: hash(reportRaw),
  })
  p.records[0].endRaw = json(end)
  const finished = JSON.parse(p.finishedRaw)
  finished.records[0].endSha256 = hash(p.records[0].endRaw)
  p.finishedRaw = json(finished)
  assert.equal(auditRecordedRun(p).records, 1)
  assert.throws(() =>
    auditRecordedRun({
      ...p,
      records: [{ ...p.records[0], groups: [p.records[0].groups[0]] }],
    }),
  )
  assert.throws(() =>
    auditRecordedRun({
      ...p,
      records: [{ ...p.records[0], groups: [...p.records[0].groups].reverse() }],
    }),
  )
})

test('rejects absent, overwritten, duplicate or unbound raw evidence', () => {
  for (const mutate of [
    (p) => {
      p.records[0].groups[0].reportRaw = null
    },
    (p) => {
      p.records[0].groups[0].reportRaw += ' '
    },
    (p) => {
      p.records.push(p.records[0])
    },
    (p) => {
      p.records[0].endRaw += ' '
    },
    (p) => {
      p.resultsRaw += ' '
    },
  ]) {
    const p = packageEvidence(fixture())
    mutate(p)
    assert.throws(() => auditRecordedRun(p))
  }
})

test('rejects a non-normal or unpinned baseline', () => {
  rejects((f) => {
    f.baseline.reports.example.report['example.read'].listed = 0
  })
  rejects((f) => {
    f.expected.baselineSha256 = '0'.repeat(64)
  })
})
