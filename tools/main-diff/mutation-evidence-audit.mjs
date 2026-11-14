import { createHash } from 'node:crypto'
import { isAbsolute } from 'node:path'
import { isDeepStrictEqual as same } from 'node:util'
import { auditNormalBaseline } from './recovery-mutation.mjs'

const hash = (bytes) => createHash('sha256').update(bytes).digest('hex')
const objectHash = (value) => hash(JSON.stringify(value))
const digest = (value) => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value)
const key = (fn, index) => JSON.stringify([fn, index])
const requireValue = (value, message) => {
  if (!value) throw new Error(message)
}

function bytes(value) {
  requireValue(typeof value === 'string' || Buffer.isBuffer(value), 'Missing raw file')
  return Buffer.isBuffer(value) ? value : Buffer.from(value, 'utf8')
}

function parse(raw) {
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes(raw)))
}

function validateWire(value) {
  const object = (item) => item !== null && typeof item === 'object' && !Array.isArray(item)
  requireValue(
    object(value) &&
      typeof value.answer === 'string' &&
      object(value.files) &&
      Object.values(value.files).every((file) => typeof file === 'string'),
    'Invalid typed wire',
  )
  requireValue(object(JSON.parse(value.answer)), 'Invalid typed answer')
}

function outputDiagnostics(command) {
  requireValue(
    typeof command.stdout === 'string' && typeof command.stderr === 'string',
    'Missing comparison output',
  )
  const text = `${command.stdout}\n${command.stderr}`
  requireValue(
    !/\bFLOOR: fewer than two distinct answers\b/.test(text) &&
      !/^(?:no such group:|unknown (?:group|function|selector)\b|fatal(?::|\s+error\b)|(?:[A-Za-z_$][\w$]*Error|Error)(?:\s+\[[^\]]+\])?(?::|\s+))/im.test(
        text,
      ),
    'Fatal comparison diagnostic',
  )
  const stale = [
    ...text.matchAll(/^STALE ledger entry ([^:\r\n]+): it no longer reproduces\s*$/gm),
  ].map((m) => m[1])
  requireValue(
    !/^STALE\b(?! ledger entry [^:\r\n]+: it no longer reproduces\s*$)/m.test(text) &&
      new Set(stale).size === stale.length,
    'Invalid STALE diagnostic',
  )
  const diffs = [...text.matchAll(/^DIFF (\S+) case (\d+)(?:\s|$)/gm)].map((m) =>
    key(m[1], Number(m[2])),
  )
  requireValue(
    !/^DIFF\b(?! \S+ case \d+(?:\s|$))/m.test(text) && new Set(diffs).size === diffs.length,
    'Invalid DIFF diagnostic',
  )
  return { stale, diffs }
}

function inspectMutationReport(raw, command, inventory, ledger) {
  const payload = parse(raw)
  requireValue(
    payload &&
      payload.report &&
      typeof payload.report === 'object' &&
      !Array.isArray(payload.report) &&
      Array.isArray(payload.differences),
    'Invalid report',
  )
  requireValue(
    same(Object.keys(payload.report).sort(), inventory.map((f) => f.name).sort()),
    'Report inventory mismatch',
  )
  const diagnostics = outputDiagnostics(command)
  const byName = new Map(inventory.map((f) => [f.name, f]))
  const byKey = new Map(ledger.map((entry) => [key(entry.fn, entry.case), entry]))
  const byId = new Map(ledger.map((entry) => [entry.id, entry]))
  const differences = new Map()
  for (const diff of payload.differences) {
    const fn = byName.get(diff?.fn)
    const id = key(diff?.fn, diff?.case)
    requireValue(
      fn &&
        Number.isSafeInteger(diff.case) &&
        diff.case >= 0 &&
        diff.case < fn.checks &&
        !differences.has(id) &&
        typeof diff.typescript === 'string' &&
        typeof diff.rust === 'string' &&
        diff.typescript !== diff.rust,
      'Invalid typed difference',
    )
    validateWire(JSON.parse(diff.typescript))
    validateWire(JSON.parse(diff.rust))
    differences.set(id, diff)
  }
  requireValue(
    diagnostics.diffs.every((id) => differences.has(id)) &&
      diagnostics.stale.every((id) => byId.has(id)),
    'Unmatched diagnostic',
  )
  for (const id of diagnostics.stale) {
    const entry = byId.get(id)
    const diff = differences.get(key(entry.fn, entry.case))
    requireValue(
      diff &&
        diff.typescript === JSON.stringify(entry.typescript) &&
        diff.rust !== JSON.stringify(entry.rust) &&
        diff.rust !== diff.typescript,
      'STALE lacks changed typed tuple',
    )
  }
  for (const [id] of differences) {
    const entry = byKey.get(id)
    requireValue(!entry || diagnostics.stale.includes(entry.id), 'Ledger difference without STALE')
  }
  for (const fn of inventory) {
    const report = payload.report[fn.name]
    const count = [...differences.values()].filter((item) => item.fn === fn.name).length
    const entries = ledger.filter((item) => item.fn === fn.name)
    const staleCount = entries.filter((item) => diagnostics.stale.includes(item.id)).length
    requireValue(
      report &&
        report.checks === fn.checks &&
        Number.isSafeInteger(report.equal) &&
        report.equal >= 0 &&
        Number.isSafeInteger(report.listed) &&
        report.listed >= 0 &&
        Number.isSafeInteger(report.distinct) &&
        report.distinct >= 1 &&
        report.distinct <= fn.checks &&
        (fn.single || report.distinct >= 2) &&
        report.equal + report.listed + count === fn.checks &&
        report.listed === entries.length - staleCount,
      'Invalid report counts or FLOOR',
    )
    const line = `${fn.name}: ${report.checks} checks, ${count} differences, ${report.listed} listed, ${report.distinct} distinct`
    requireValue(
      command.stdout.split(/\r?\n/).filter((item) => item === line).length === 1,
      'Comparison summary mismatch',
    )
  }
  requireValue(
    command.signal === null &&
      command.status === (differences.size || diagnostics.stale.length ? 1 : 0),
    'Comparison status mismatch',
  )
  return {
    independent: [...differences].filter(([id]) => !byKey.has(id)),
    displayed: diagnostics.diffs,
  }
}

export function auditRetainedBaseline(baseline) {
  auditNormalBaseline(baseline)
  for (const command of baseline.commands) outputDiagnostics(command)
  for (const entry of baseline.ledger) {
    validateWire(entry.typescript)
    validateWire(entry.rust)
  }
}

function checkedBaseline(baseline, expected) {
  requireValue(
    digest(expected.baselineSha256) && objectHash(baseline) === expected.baselineSha256,
    'Baseline pin mismatch',
  )
  requireValue(
    same(baseline.identity, expected.baselineIdentity) &&
      same(baseline.sources, expected.baselineSources) &&
      baseline.sources.mutatorSha256 === expected.modules?.['tools/main-diff/faults.mjs'] &&
      ['productSha', 'definitionSha', 'runId', 'attempt'].every(
        (field) => baseline.identity[field] === expected.identity?.[field],
      ),
    'Baseline identity or source mismatch',
  )
  auditRetainedBaseline(baseline)
}

function validateRun(run, expected) {
  requireValue(
    expected &&
      expected.identity &&
      /^[a-f0-9]{40}$/.test(expected.identity.productSha) &&
      /^[a-f0-9]{40}$/.test(expected.identity.definitionSha) &&
      typeof expected.identity.runId === 'string' &&
      /^[1-9]\d*$/.test(expected.identity.runId) &&
      Number.isSafeInteger(expected.identity.attempt) &&
      expected.identity.attempt > 0,
    'Invalid expected identity',
  )
  const paths = ['tools/main-diff/faults.mjs', 'tools/main-diff/mutation-evidence.mjs']
  requireValue(
    expected.modules &&
      same(Object.keys(expected.modules).sort(), [...paths].sort()) &&
      paths.every((path) => digest(expected.modules[path])),
    'Invalid expected producer modules',
  )
  requireValue(
    run.contract === 'per-mutation-producer-evidence/v1' &&
      same(run.identity, expected.identity) &&
      same(run.producer, { modules: expected.modules }) &&
      same(run.buildEnvironment, {
        profile: 'node-faults',
        cargoIncremental: '1',
      }) &&
      same(
        run.catalogPaths,
        expected.catalogs.map((catalog) => catalog.path),
      ) &&
      run.shard === expected.shard &&
      run.shards === expected.shards &&
      Number.isSafeInteger(run.shards) &&
      run.shards > 0 &&
      Number.isSafeInteger(run.shard) &&
      run.shard >= 0 &&
      run.shard < run.shards,
    'Run identity mismatch',
  )
  requireValue(
    isAbsolute(expected.cwd) &&
      expected.catalogs.length > 0 &&
      new Set(run.catalogPaths).size === run.catalogPaths.length,
    'Invalid expected catalog or cwd',
  )
}

function auditRecord({ expected, catalog, index, record, baseline }) {
  const fault = catalog.faults[index]
  const start = parse(record.startRaw)
  const applied = parse(record.appliedRaw)
  const end = parse(record.endRaw)
  requireValue(
    fault &&
      typeof fault.file === 'string' &&
      fault.file.length > 0 &&
      !fault.file.split('/').some((part) => part === '..' || part === '' || part === '.') &&
      typeof fault.find === 'string' &&
      fault.find.length > 0 &&
      typeof fault.replace === 'string' &&
      Array.isArray(fault.fn) &&
      fault.fn.length > 0 &&
      fault.fn.every((fn) => typeof fn === 'string' && fn.length > 0) &&
      new Set(fault.fn).size === fault.fn.length,
    'Invalid fault',
  )
  const path = `crates/${fault.file}`
  const original = expected.sources[path]
  requireValue(
    typeof original === 'string' && original.split(fault.find).length - 1 === 1,
    'Invalid original source',
  )
  const before = hash(original)
  const mutated = hash(original.replace(fault.find, fault.replace))
  requireValue(
    before !== mutated &&
      same(start.catalog, catalog.metadata) &&
      start.index === index &&
      start.mutationSha256 === objectHash(fault) &&
      same(start.fault, fault) &&
      start.isControl === (index === 0) &&
      same(start.source, {
        path,
        exists: true,
        beforeSha256: before,
        findCount: 1,
        derivedAppliedSourceSha256: mutated,
      }) &&
      same(applied, { path, observedSha256: mutated }),
    'Mutation source or identity mismatch',
  )
  const byName = new Map(baseline.functions.map((fn) => [fn.name, fn]))
  requireValue(
    fault.fn.every((fn) => byName.has(fn)),
    'Unknown selector',
  )
  const groups = [...new Set(fault.fn.map((fn) => byName.get(fn).group))]
  requireValue(
    Array.isArray(record.groups) && record.groups.length === groups.length,
    'Missing comparison groups',
  )
  let independent = 0
  let combined = ''
  const bindings = []
  for (let position = 0; position < groups.length; position++) {
    const input = record.groups[position]
    const command = parse(input.commandRaw)
    const group = groups[position]
    const functions = fault.fn.filter((fn) => byName.get(fn).group === group)
    const args = [
      'tsx',
      'main.ts',
      '--group',
      group,
      '--show',
      '3',
      ...functions.flatMap((fn) => ['--fn', fn]),
    ]
    requireValue(
      command.position === position &&
        command.group === group &&
        same(command.functions, functions) &&
        command.command === 'npx' &&
        same(command.args, args) &&
        command.cwd === expected.cwd &&
        command.sourceSha256AtCompare === mutated &&
        command.reportSha256 === hash(bytes(input.reportRaw)),
      'Comparison command or report mismatch',
    )
    const inventory = baseline.functions.filter((fn) => functions.includes(fn.name))
    const ledger = baseline.ledger.filter((entry) => functions.includes(entry.fn))
    const result = inspectMutationReport(input.reportRaw, command, inventory, ledger)
    independent += result.independent.length
    requireValue(
      result.displayed.length > 0 || result.independent.length === 0,
      'Missing DIFF output',
    )
    combined += `${command.stdout}${command.stderr}`
    bindings.push({
      position,
      commandSha256: hash(bytes(input.commandRaw)),
      reportSha256: command.reportSha256,
    })
  }
  const detail = combined
    .split('\n')
    .filter(
      (line) =>
        line.startsWith('DIFF ') ||
        / checks, \d+ equal/.test(line) ||
        /FLOOR|STALE|Error/.test(line),
    )
    .slice(0, 4)
    .join(' | ')
  requireValue(
    independent > 0 &&
      end.startSha256 === hash(bytes(record.startRaw)) &&
      end.appliedSha256 === hash(bytes(record.appliedRaw)) &&
      same(end.groups, bindings) &&
      end.verdict === 'CAUGHT' &&
      end.detail === detail &&
      same(end.sourceAfter, { exists: true, sha256: before }) &&
      end.restored === true,
    'Incomplete or insensitive mutation',
  )
  return {
    faults: catalog.metadata.path,
    name: fault.name,
    file: fault.file,
    fn: fault.fn,
    verdict: end.verdict,
    detail,
  }
}

export function auditRecordedRun({
  expected,
  runRaw,
  finishedRaw,
  catalogsRaw,
  records,
  baseline,
  resultsRaw,
}) {
  checkedBaseline(baseline, expected)
  const run = parse(runRaw)
  validateRun(run, expected)
  requireValue(
    Array.isArray(catalogsRaw) &&
      catalogsRaw.length === expected.catalogs.length &&
      Array.isArray(records),
    'Incomplete run inventory',
  )
  const catalogs = expected.catalogs.map((item, ordinal) => {
    const raw = bytes(item.raw)
    const faults = parse(raw)
    const metadata = {
      ordinal,
      path: item.path,
      sha256: hash(raw),
      length: faults.length,
    }
    requireValue(
      Array.isArray(faults) && faults.length > 0 && same(parse(catalogsRaw[ordinal]), metadata),
      'Catalog mismatch',
    )
    return { metadata, faults }
  })
  const selected = catalogs.flatMap((catalog, catalogOrdinal) =>
    catalog.faults.flatMap((_, index) =>
      index === 0 || index % run.shards === run.shard ? [{ catalogOrdinal, index }] : [],
    ),
  )
  requireValue(records.length === selected.length, 'Missing or duplicate mutation records')
  const rows = []
  const recordBindings = []
  for (let i = 0; i < selected.length; i++) {
    const { catalogOrdinal, index } = selected[i]
    const record = records[i]
    requireValue(
      record?.catalogOrdinal === catalogOrdinal && record.index === index,
      'Mutation order mismatch',
    )
    rows.push(
      auditRecord({
        expected,
        catalog: catalogs[catalogOrdinal],
        index,
        record,
        baseline,
      }),
    )
    recordBindings.push({
      catalogOrdinal,
      index,
      startSha256: hash(bytes(record.startRaw)),
      endSha256: hash(bytes(record.endRaw)),
    })
  }
  const finished = parse(finishedRaw)
  requireValue(
    finished.runSha256 === hash(bytes(runRaw)) &&
      same(
        finished.catalogs,
        catalogs.map(({ metadata }) => ({
          ordinal: metadata.ordinal,
          sha256: metadata.sha256,
        })),
      ) &&
      same(finished.records, recordBindings) &&
      finished.resultsSha256 === hash(bytes(resultsRaw)) &&
      finished.finalBuildCompleted === true &&
      same(parse(resultsRaw), rows),
    'Finished record mismatch',
  )
  return { scope: 'per-mutation-producer-evidence/v1', records: rows.length }
}
