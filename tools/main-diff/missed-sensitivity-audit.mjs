import { isDeepStrictEqual as same } from 'node:util'
import { hash, hashBytes } from './recovery-plan.mjs'
import { auditNormalBaseline } from './recovery-mutation.mjs'
import { originalMutatorSha256 } from './recovery-runner-model.mjs'

const sha = (s) => typeof s === 'string' && /^[a-f0-9]{40}$/.test(s)
const sha256 = (s) => typeof s === 'string' && /^[a-f0-9]{64}$/.test(s)
const text = (s) => typeof s === 'string' && s.length > 0
const positive = (n) => Number.isSafeInteger(n) && n > 0
const oldScope = 'five-miss-followup-only-not-full-recovery'
const ledgerScope = 'ledger-consumer-followup-only-not-full-recovery'
const suiteScope = 'ledger-consumer-suite-only-not-full-recovery'
const chatgptScope = 'chatgpt-consumer-followup-only-not-full-recovery'
const scopes = [oldScope, ledgerScope, suiteScope, chatgptScope]
const keyOf = (fn, index) => JSON.stringify([fn, index])
const requireValue = (condition, message) => {
  if (!condition) throw new Error(message)
}
const safePath = (s) =>
  text(s) && !s.startsWith('/') && !s.split('/').some((p) => !p || p === '.' || p === '..')
const selectors = (f) =>
  Array.isArray(f?.fn) && f.fn.length > 0 && f.fn.every(text) && new Set(f.fn).size === f.fn.length

function validFault(f) {
  return (
    f &&
    text(f.name) &&
    safePath(f.file) &&
    text(f.find) &&
    typeof f.replace === 'string' &&
    f.find !== f.replace &&
    selectors(f)
  )
}

function validWire(wire) {
  if (typeof wire !== 'string') return false
  try {
    const parsed = JSON.parse(wire)
    if (
      !parsed ||
      typeof parsed !== 'object' ||
      Array.isArray(parsed) ||
      typeof parsed.answer !== 'string' ||
      !parsed.files ||
      typeof parsed.files !== 'object' ||
      Array.isArray(parsed.files)
    )
      return false
    const answer = JSON.parse(parsed.answer)
    return answer !== null && typeof answer === 'object' && !Array.isArray(answer)
  } catch {
    return false
  }
}

export function validateManifest(manifest) {
  requireValue(
    manifest?.contract === 'main-missed-sensitivity-manifest/v1' &&
      sha(manifest.productSha) &&
      manifest.productSha !== manifest.parentProductSha &&
      manifest.parentProductSha === '375b08bd0e1f7e8432e6514468cc9d25be01b100' &&
      same(manifest.originalRun, {
        runId: '43604410368',
        attempt: 1,
        definitionSha: '97ffeb5ec856c6a05399eae70525ede3568057ba',
        productSha: manifest.parentProductSha,
      }) &&
      manifest.mutatorSha256 === originalMutatorSha256 &&
      scopes.includes(manifest.scope),
    'Invalid manifest identity',
  )
  requireValue(
    Array.isArray(manifest.sourceLineage?.paths) && manifest.sourceLineage.paths.length > 0,
    'Missing source lineage',
  )
  const paths = new Set()
  for (const item of manifest.sourceLineage.paths) {
    requireValue(
      safePath(item?.path) &&
        !paths.has(item.path) &&
        (item.parentSha256 === null || sha256(item.parentSha256)) &&
        sha256(item.productSha256) &&
        item.parentSha256 !== item.productSha256,
      'Invalid source lineage',
    )
    paths.add(item.path)
  }
  requireValue(
    Array.isArray(manifest.cases) &&
      manifest.cases.length > 0 &&
      manifest.cases.length <= (manifest.scope === suiteScope ? 256 : 8),
    'Invalid manifest cases',
  )
  const ids = new Set(),
    indices = new Set(),
    catalogs = new Map()
  for (const c of manifest.cases) {
    requireValue(
      text(c?.id) &&
        !ids.has(c.id) &&
        /^tools\/main-diff\/faults\/[^/]+\.json$/.test(c.catalog ?? '') &&
        positive(c.index) &&
        positive(c.catalogLength) &&
        c.index < c.catalogLength &&
        !indices.has(`${c.catalog}#${c.index}`),
      'Invalid or duplicate case',
    )
    ids.add(c.id)
    indices.add(`${c.catalog}#${c.index}`)
    requireValue(
      sha256(c.catalogSha256) &&
        sha256(c.originalMutationSha256) &&
        validFault(c.fault) &&
        validFault(c.control) &&
        c.mutationSha256 === hash(c.fault) &&
        c.controlSha256 === hash(c.control),
      'Invalid mutation or control hash',
    )
    if (c.mutationLineage === undefined) {
      requireValue(
        c.originalMutationSha256 === c.mutationSha256,
        'Changed mutation requires lineage',
      )
    } else {
      const lineage = c.mutationLineage
      requireValue(
        lineage &&
          validFault(lineage.originalFault) &&
          lineage.originalSha256 === c.originalMutationSha256 &&
          lineage.originalSha256 === hash(lineage.originalFault) &&
          lineage.followupSha256 === c.mutationSha256 &&
          text(lineage.reason) &&
          lineage.originalSha256 !== lineage.followupSha256,
        'Invalid mutation lineage',
      )
      const {
        name: originalName,
        replace: originalReplace,
        ...originalRest
      } = lineage.originalFault
      const { name, replace, ...followupRest } = c.fault
      requireValue(
        same(originalRest, followupRest) && originalName !== name && originalReplace !== replace,
        'Invalid mutation lineage change',
      )
    }
    const known = catalogs.get(c.catalog)
    const catalogIdentity = {
      catalogSha256: c.catalogSha256,
      catalogLength: c.catalogLength,
      controlSha256: c.controlSha256,
    }
    requireValue(!known || same(known, catalogIdentity), 'Inconsistent catalog control')
    catalogs.set(c.catalog, catalogIdentity)
    const r = c.caseRequirement
    requireValue(
      r &&
        c.fault.fn.includes(r.fn) &&
        Number.isSafeInteger(r.index) &&
        r.index >= 0 &&
        (manifest.scope === chatgptScope ? r.note === undefined : text(r.note)) &&
        validWire(r.typescript),
      'Invalid case requirement',
    )
    const ledger = c.ledgerRequirements ?? []
    requireValue(
      Array.isArray(ledger) &&
        (manifest.scope === oldScope || manifest.scope === chatgptScope
          ? ledger.length === 0
          : ledger.length > 0) &&
        (manifest.scope !== ledgerScope || manifest.cases.length === 1) &&
        ledger.every(
          (entry) =>
            text(entry?.id) &&
            c.fault.fn.includes(entry.fn) &&
            Number.isSafeInteger(entry.case) &&
            entry.case >= 0 &&
            validWire(JSON.stringify(entry.typescript)) &&
            validWire(JSON.stringify(entry.rust)) &&
            !same(entry.typescript, entry.rust) &&
            !(entry.fn === r.fn && entry.case === r.index),
        ) &&
        new Set(ledger.map((entry) => entry.id)).size === ledger.length &&
        new Set(ledger.map((entry) => keyOf(entry.fn, entry.case))).size === ledger.length,
      'Invalid ledger requirements',
    )
    if (manifest.scope === ledgerScope) {
      const change = r.valueChange
      requireValue(
        change &&
          Array.isArray(change.path) &&
          change.path.length > 0 &&
          change.path.every((part) => text(part) || (Number.isSafeInteger(part) && part >= 0)) &&
          Object.hasOwn(change, 'before') &&
          Object.hasOwn(change, 'after') &&
          !same(change.before, change.after),
        'Invalid value change requirement',
      )
    } else requireValue(r.valueChange === undefined, 'Unexpected value change requirement')
    if (manifest.scope === suiteScope || manifest.scope === chatgptScope) {
      requireValue(
        validWire(r.expectedRust) && !same(JSON.parse(r.typescript), JSON.parse(r.expectedRust)),
        'Invalid expected consumer result',
      )
    } else requireValue(r.expectedRust === undefined, 'Unexpected expected consumer result')
    requireValue(
      Array.isArray(c.baselineExpected) && c.baselineExpected.length > 0,
      'Missing baseline inventory',
    )
    const names = new Set()
    for (const f of c.baselineExpected) {
      requireValue(
        text(f?.group) &&
          text(f.name) &&
          !names.has(f.name) &&
          positive(f.checks) &&
          typeof f.single === 'boolean',
        'Invalid baseline inventory',
      )
      names.add(f.name)
    }
    requireValue(
      same([...names].sort(), [...new Set([...c.control.fn, ...c.fault.fn])].sort()) &&
        c.baselineExpected.find((f) => f.name === r.fn).checks > r.index &&
        ledger.every(
          (entry) => entry.case < c.baselineExpected.find((f) => f.name === entry.fn).checks,
        ) &&
        new Set(c.baselineExpected.filter((f) => c.fault.fn.includes(f.name)).map((f) => f.group))
          .size === 1,
      'Case or selector inventory mismatch',
    )
  }
  return true
}

export function makePlan(manifest, definitionSha) {
  validateManifest(manifest)
  requireValue(sha(definitionSha), 'Invalid definition SHA')
  return {
    contract: 'main-missed-sensitivity-plan/v1',
    definitionSha,
    productSha: manifest.productSha,
    manifestSha256: hash(manifest),
    cases: manifest.cases.map((c) => ({
      ...c,
      modulus: 4 * Math.ceil(c.catalogLength / 4),
      residue: c.index,
    })),
    maxParallel: 8,
    scope: manifest.scope,
  }
}

export function auditBaseline(expected, baseline) {
  requireValue(
    expected &&
      same(expected.identity, baseline?.identity) &&
      same(expected.baselineExpected, baseline.functions) &&
      Array.isArray(expected.ledgerRequirements ?? []) &&
      same(baseline.ledger, expected.ledgerRequirements ?? []) &&
      baseline.sources?.mutatorSha256 === originalMutatorSha256,
    'Baseline identity or inventory mismatch',
  )
  return auditNormalBaseline(baseline)
}

function detailDiffs(detail, fns, allowStale = false) {
  requireValue(text(detail), 'Missing comparison detail')
  const segments = []
  let start = 0,
    quoted = false,
    escaped = false
  for (let i = 0; i < detail.length; i++) {
    const ch = detail[i]
    if (quoted && escaped) escaped = false
    else if (quoted && ch === '\\') escaped = true
    else if (ch === '"') quoted = !quoted
    if (!quoted && detail.startsWith(' | ', i)) {
      segments.push(detail.slice(start, i))
      i += 2
      start = i + 1
    }
  }
  segments.push(detail.slice(start))
  const found = []
  const stale = []
  for (const segment of segments.flatMap((s) => s.split(/\r?\n/))) {
    requireValue(
      !/^(?:no such group:|fatal(?::|\s+error\b)|(?:[A-Za-z_$][\w$]*Error|Error)(?:\s+\[[^\]]+\])?(?::|\s+))/i.test(
        segment,
      ) && !/\bFLOOR: fewer than two distinct answers\b/.test(segment),
      'Fatal comparison diagnostic',
    )
    if (/^STALE\b/.test(segment)) {
      const match = /^STALE ledger entry ([^:\r\n]+): it no longer reproduces$/.exec(segment)
      requireValue(allowStale && match && !stale.includes(match[1]), 'Invalid STALE diagnostic')
      stale.push(match[1])
    }
    const match = /^DIFF (\S+) case (\d+)(?:\s|$)/.exec(segment)
    requireValue(!/^DIFF\b/.test(segment) || match, 'Malformed DIFF')
    if (match) {
      requireValue(fns.includes(match[1]), 'DIFF outside selected functions')
      found.push([match[1], Number(match[2])])
    }
  }
  requireValue(
    found.length > 0 && new Set(found.map((pair) => JSON.stringify(pair))).size === found.length,
    'Missing or duplicate DIFF',
  )
  return { diffs: found, stale }
}

export function auditTarget({ plan, target, baseline, evidence }) {
  requireValue(
    plan?.contract === 'main-missed-sensitivity-plan/v1' &&
      sha(plan.definitionSha) &&
      sha(plan.productSha) &&
      sha256(plan.manifestSha256) &&
      plan.maxParallel === 8 &&
      scopes.includes(plan.scope) &&
      (plan.scope === oldScope || plan.scope === chatgptScope
        ? (target.ledgerRequirements ?? []).length === 0 &&
          (plan.scope !== chatgptScope || Array.isArray(target.ledgerRequirements ?? [])) &&
          target.caseRequirement?.valueChange === undefined &&
          (plan.scope === oldScope
            ? target.caseRequirement?.expectedRust === undefined
            : validWire(target.caseRequirement?.expectedRust) &&
              target.caseRequirement?.note === undefined &&
              plan.cases.length <= 8)
        : Array.isArray(target.ledgerRequirements) &&
          target.ledgerRequirements.length > 0 &&
          (plan.scope === ledgerScope
            ? plan.cases.length === 1 &&
              target.caseRequirement?.valueChange &&
              target.caseRequirement?.expectedRust === undefined
            : plan.cases.length <= 256 &&
              validWire(target.caseRequirement?.expectedRust) &&
              target.caseRequirement?.valueChange === undefined)) &&
      Array.isArray(plan.cases) &&
      plan.cases.filter((c) => same(c, target)).length === 1 &&
      target.modulus === 4 * Math.ceil(target.catalogLength / 4) &&
      target.residue === target.index &&
      target.index > 0 &&
      target.index < target.catalogLength &&
      hash(target.fault) === target.mutationSha256 &&
      hash(target.control) === target.controlSha256,
    'Invalid plan or target',
  )
  const identity = baseline?.identity
  requireValue(
    identity &&
      identity.definitionSha === plan.definitionSha &&
      identity.productSha === plan.productSha &&
      identity.manifestSha256 === plan.manifestSha256 &&
      identity.planSha256 === hash(plan) &&
      identity.targetId === target.id &&
      /^[1-9]\d*$/.test(String(identity.runId ?? '')) &&
      /^[1-9]\d*$/.test(String(identity.attempt ?? '')) &&
      same(identity, evidence?.identity),
    'Wrong evidence identity',
  )
  auditBaseline(
    {
      identity,
      baselineExpected: target.baselineExpected,
      ledgerRequirements: target.ledgerRequirements,
    },
    baseline,
  )
  requireValue(
    Array.isArray(evidence.rows) && evidence.rows.length === 2 && typeof evidence.text === 'string',
    'Missing result rows',
  )
  const summaries = [
    ...evidence.text.matchAll(
      /^(\d+) faults: (\d+) caught, (\d+) missed, (\d+) not building, (\d+) bad\s*$/gm,
    ),
  ]
  requireValue(
    summaries.length === 1 && same(summaries[0].slice(1).map(Number), [2, 2, 0, 0, 0]),
    'Invalid mutation summary',
  )
  const details = [target.control, target.fault].map((fault, position) => {
    const row = evidence.rows[position]
    requireValue(
      row &&
        row.faults === target.catalog &&
        row.name === fault.name &&
        row.file === fault.file &&
        same(row.fn, fault.fn) &&
        row.verdict === 'CAUGHT',
      'Invalid mutation row',
    )
    return detailDiffs(
      row.detail,
      fault.fn,
      position === 1 && (plan.scope === ledgerScope || plan.scope === suiteScope),
    )
  })
  const paths = [...new Set([target.control, target.fault].map((f) => `crates/${f.file}`))].sort()
  requireValue(
    evidence.sourceBefore &&
      evidence.sourceAfter &&
      same(Object.keys(evidence.sourceBefore).sort(), paths) &&
      same(evidence.sourceBefore, evidence.sourceAfter) &&
      Object.values(evidence.sourceBefore).every(sha256),
    'Invalid or unrestored source',
  )
  const model = evidence.sourceModel
  requireValue(
    model?.kind === 'pinned-original-runner-control-flow' &&
      model.mutatorSha256 === originalMutatorSha256 &&
      Array.isArray(model.entries) &&
      model.entries.length === 2,
    'Invalid source model',
  )
  for (const [i, fault] of [target.control, target.fault].entries()) {
    const entry = model.entries[i],
      path = `crates/${fault.file}`
    requireValue(
      entry?.index === (i === 0 ? 0 : target.index) &&
        entry.path === path &&
        entry.findCount === 1 &&
        sha256(entry.derivedAppliedSourceSha256) &&
        entry.derivedAppliedSourceSha256 !== evidence.sourceBefore[path],
      'Invalid modeled mutation',
    )
  }
  const payload = evidence.report
  requireValue(
    payload &&
      evidence.reportSha256 === hashBytes(JSON.stringify(payload, null, 2)) &&
      payload.report &&
      typeof payload.report === 'object' &&
      !Array.isArray(payload.report) &&
      Array.isArray(payload.differences) &&
      same(Object.keys(payload.report).sort(), [...target.fault.fn].sort()),
    'Invalid terminal report',
  )
  const seen = new Set()
  for (const diff of payload.differences) {
    const fn = target.baselineExpected.find(
      (f) => f.name === diff?.fn && target.fault.fn.includes(f.name),
    )
    const key = JSON.stringify([diff?.fn, diff?.case])
    requireValue(
      fn &&
        Number.isSafeInteger(diff.case) &&
        diff.case >= 0 &&
        diff.case < fn.checks &&
        !seen.has(key) &&
        validWire(diff.typescript) &&
        validWire(diff.rust) &&
        diff.typescript !== diff.rust,
      'Invalid typed difference',
    )
    seen.add(key)
  }
  requireValue(
    details[1].diffs.every((pair) => seen.has(JSON.stringify(pair))),
    'Printed DIFF absent from report',
  )
  for (const name of target.fault.fn) {
    const fn = target.baselineExpected.find((f) => f.name === name),
      r = payload.report[name]
    const count = payload.differences.filter((d) => d.fn === name).length
    requireValue(
      r &&
        r.checks === fn.checks &&
        Number.isSafeInteger(r.equal) &&
        r.equal >= 0 &&
        r.listed === 0 &&
        Number.isSafeInteger(r.distinct) &&
        r.distinct >= (fn.single ? 1 : 2) &&
        r.distinct <= r.checks &&
        r.equal + count === r.checks,
      'Invalid terminal report counts or FLOOR',
    )
  }
  const ledger = target.ledgerRequirements ?? []
  for (const entry of ledger) {
    const diff = payload.differences.find((d) => d.fn === entry.fn && d.case === entry.case)
    requireValue(
      diff &&
        diff.typescript === JSON.stringify(entry.typescript) &&
        diff.rust !== JSON.stringify(entry.rust) &&
        diff.rust !== diff.typescript,
      'Missing or unchanged typed ledger tuple',
    )
  }
  requireValue(
    same([...details[1].stale].sort(), ledger.map((entry) => entry.id).sort()),
    'STALE diagnostics do not match typed ledger requirements',
  )
  const requirement = target.caseRequirement
  const actual = payload.differences.find(
    (d) => d.fn === requirement.fn && d.case === requirement.index,
  )
  requireValue(
    actual && actual.note === requirement.note && actual.typescript === requirement.typescript,
    'Missing appended case typed difference or wrong TypeScript oracle',
  )
  requireValue(
    !ledger.some((entry) => entry.fn === actual.fn && entry.case === actual.case),
    'Required consumer case must be independent of ledger',
  )
  if (plan.scope === suiteScope || plan.scope === chatgptScope) {
    requireValue(
      plan.scope === chatgptScope
        ? actual.rust === requirement.expectedRust
        : same(JSON.parse(actual.rust), JSON.parse(requirement.expectedRust)),
      'Wrong exact consumer result',
    )
  }
  if (requirement.valueChange) {
    const ts = JSON.parse(actual.typescript)
    const rust = JSON.parse(actual.rust)
    const answer = JSON.parse(ts.answer)
    const mutant = JSON.parse(rust.answer)
    const { path, before, after } = requirement.valueChange
    let parent = answer
    for (const part of path.slice(0, -1)) {
      requireValue(
        parent !== null && typeof parent === 'object' && Object.hasOwn(parent, part),
        'Missing value change path',
      )
      parent = parent[part]
    }
    const last = path.at(-1)
    requireValue(
      parent !== null &&
        typeof parent === 'object' &&
        Object.hasOwn(parent, last) &&
        same(parent[last], before),
      'Wrong original value',
    )
    parent[last] = after
    requireValue(
      same(answer, mutant) &&
        same(ts.files, rust.files) &&
        same(Object.keys(ts).sort(), Object.keys(rust).sort()) &&
        Object.keys(ts).every((key) => key === 'answer' || same(ts[key], rust[key])),
      'Wrong consumer value change',
    )
  }
  return {
    scope:
      plan.scope === suiteScope
        ? 'ledger-consumer-suite-target-only'
        : plan.scope === chatgptScope
          ? 'chatgpt-consumer-target-only'
          : 'missed-sensitivity-only',
    identity,
    target,
    records: evidence.rows,
    reportSha256: evidence.reportSha256,
    typedNewCaseDifference: actual,
    baseline,
    evidence,
  }
}
