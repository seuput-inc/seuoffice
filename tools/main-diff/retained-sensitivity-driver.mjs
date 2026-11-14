import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { execFileSync, spawnSync } from 'node:child_process'
import {
  appendFileSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs'
import { isAbsolute, join, relative, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { auditRecordedRun, auditRetainedBaseline } from './mutation-evidence-audit.mjs'

const self = fileURLToPath(import.meta.url)
const hash = (x) => createHash('sha256').update(x).digest('hex')
const objectHash = (x) => hash(JSON.stringify(x))
const json = (path) => JSON.parse(readFileSync(path, 'utf8'))
const save = (path, value) =>
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, { flag: 'wx' })
const git = (root, ...args) => execFileSync('git', ['-C', root, ...args])
const emit = (env, name, value) => {
  if (env.GITHUB_OUTPUT) appendFileSync(env.GITHUB_OUTPUT, `${name}=${value}\n`)
}
const sha = (x) => typeof x === 'string' && /^[a-f0-9]{40}$/.test(x)
const digest = (x) => typeof x === 'string' && /^[a-f0-9]{64}$/.test(x)
const safe = (path) =>
  typeof path === 'string' &&
  path.length > 0 &&
  !isAbsolute(path) &&
  !path.split(/[\\/]/).some((part) => !part || part === '..' || part === '.')

export function proofIdentity(env, manifest) {
  assert.equal(env.MODE, 'main-retained-producer-proof')
  assert.ok(sha(env.DEFINITION_SHA) && sha(env.PRODUCT_SHA))
  assert.equal(env.PRODUCT_SHA, manifest.productSha)
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

export function retainedPlan(manifest, definitionSha) {
  assert.equal(manifest.contract, 'retained-sensitivity-manifest/v1')
  assert.equal(manifest.scope, 'retained-producer-bounded-proof-only')
  assert.ok(sha(manifest.productSha) && sha(manifest.parentProductSha) && sha(definitionSha))
  assert.notEqual(manifest.productSha, manifest.parentProductSha)
  assert.ok(safe(manifest.catalog) && digest(manifest.catalogSha256))
  assert.ok(Number.isSafeInteger(manifest.catalogLength) && manifest.catalogLength > 1)
  assert.ok(
    Number.isSafeInteger(manifest.shards) && manifest.shards > 0 && manifest.shards % 4 === 0,
  )
  assert.ok(
    Number.isSafeInteger(manifest.shard) && manifest.shard >= 0 && manifest.shard < manifest.shards,
  )
  const indices = Array.from({ length: manifest.catalogLength }, (_, i) => i).filter(
    (i) => i === 0 || i % manifest.shards === manifest.shard,
  )
  assert.deepEqual(manifest.indices, indices)
  assert.ok(indices.length > 2 && indices.length <= 25)
  assert.ok(
    indices.includes(manifest.focus.index) &&
      manifest.focus.index > 0 &&
      indices.at(-1) > manifest.focus.index,
  )
  assert.ok(typeof manifest.focus.fn === 'string' && manifest.focus.fn.length > 0)
  assert.ok(
    Number.isSafeInteger(manifest.focus.ledgerCase) &&
      Number.isSafeInteger(manifest.focus.independentCase) &&
      manifest.focus.ledgerCase !== manifest.focus.independentCase,
  )
  assert.equal(typeof manifest.focus.typescript, 'string')
  assert.equal(typeof manifest.focus.rust, 'string')
  assert.deepEqual(Object.keys(manifest.modules).sort(), [
    'tools/main-diff/faults.mjs',
    'tools/main-diff/mutation-evidence.mjs',
  ])
  assert.ok(Object.values(manifest.modules).every(digest))
  assert.equal(manifest.entries.length, indices.length)
  for (const [position, entry] of manifest.entries.entries()) {
    assert.equal(entry.index, indices[position])
    assert.ok(digest(entry.mutationSha256))
  }
  assert.ok(Array.isArray(manifest.baselineExpected) && manifest.baselineExpected.length > 0)
  assert.ok(Array.isArray(manifest.ledgerRequirements))
  assert.ok(Array.isArray(manifest.sourceLineage) && manifest.sourceLineage.length > 0)
  return {
    ...manifest,
    contract: 'retained-sensitivity-plan/v1',
    definitionSha,
    manifestSha256: objectHash(manifest),
  }
}

function checkedFile(product, path) {
  assert.ok(safe(path))
  const raw = readFileSync(join(product, path))
  assert.deepEqual(raw, git(product, 'show', `HEAD:${path}`), `Uncommitted product ${path}`)
  return raw
}

function bindProduct(product, plan) {
  assert.equal(git(product, 'rev-parse', 'HEAD').toString().trim(), plan.productSha)
  const paths = git(product, 'diff', '--name-only', plan.parentProductSha, plan.productSha)
    .toString()
    .trim()
    .split('\n')
    .filter(Boolean)
  assert.deepEqual(paths.sort(), plan.sourceLineage.map((p) => p.path).sort())
  for (const item of plan.sourceLineage) {
    assert.ok(safe(item.path) && digest(item.productSha256))
    const present = git(product, 'ls-tree', plan.parentProductSha, '--', item.path).length > 0
    const before = present
      ? hash(git(product, 'show', `${plan.parentProductSha}:${item.path}`))
      : null
    assert.equal(before, item.parentSha256)
    assert.equal(hash(checkedFile(product, item.path)), item.productSha256)
  }
  for (const [path, expected] of Object.entries(plan.modules))
    assert.equal(hash(checkedFile(product, path)), expected)
  const raw = checkedFile(product, plan.catalog)
  assert.equal(hash(raw), plan.catalogSha256)
  assert.deepEqual(raw, git(product, 'show', `${plan.parentProductSha}:${plan.catalog}`))
  const faults = JSON.parse(raw)
  assert.equal(faults.length, plan.catalogLength)
  for (const entry of plan.entries)
    assert.equal(objectHash(faults[entry.index]), entry.mutationSha256)
  return { raw, faults }
}

async function inventoryGroup(product, name) {
  await import(pathToFileURL(join(product, 'packages/engine-wasm/src/node.ts')))
  const { groups } = await import(pathToFileURL(join(product, 'tools/main-diff/groups/index.ts')))
  assert.ok(Object.hasOwn(groups, name))
  const { group } = await groups[name]()
  return Object.entries(group).map(([fn, spec]) => ({
    group: name,
    name: fn,
    checks: spec.cases.length,
    single: Boolean(spec.single),
  }))
}

function baseline(product, plan, catalog, identity, out, env) {
  const list = execFileSync(
    process.execPath,
    [
      join(product, 'node_modules/tsx/dist/cli.mjs'),
      join(product, 'tools/main-diff/main.ts'),
      '--list',
    ],
    { cwd: product, env, encoding: 'utf8', maxBuffer: 64 << 20 },
  )
  const groupOf = new Map(
    list
      .split('\n')
      .filter(Boolean)
      .map((line) => line.split('\t').reverse()),
  )
  const names = [...new Set(plan.indices.flatMap((i) => catalog.faults[i].fn))].sort()
  assert.deepEqual(names, plan.baselineExpected.map((f) => f.name).sort())
  const groups = new Map()
  for (const name of names) {
    assert.ok(groupOf.has(name), `Unknown selector ${name}`)
    const group = groupOf.get(name)
    if (!groups.has(group)) groups.set(group, [])
    groups.get(group).push(name)
  }
  const ledger = json(join(product, 'tools/main-diff/divergences.json')).filter((entry) =>
    names.includes(entry.fn),
  )
  assert.deepEqual(ledger, plan.ledgerRequirements)
  const root = join(out, 'baseline')
  mkdirSync(root, { recursive: true })
  const functions = []
  for (const [group, selected] of groups) {
    const raw = execFileSync(
      process.execPath,
      [join(product, 'node_modules/tsx/dist/cli.mjs'), self, 'inventory-group', product, group],
      { cwd: product, env, encoding: 'utf8', maxBuffer: 64 << 20 },
    )
    const inventory = JSON.parse(raw)
    functions.push(...inventory.filter((f) => selected.includes(f.name)))
    save(join(root, `${group}-inventory.json`), inventory)
  }
  functions.sort(
    (a, b) =>
      plan.baselineExpected.findIndex((f) => f.name === a.name) -
      plan.baselineExpected.findIndex((f) => f.name === b.name),
  )
  assert.deepEqual(functions, plan.baselineExpected)
  const build = spawnSync(process.execPath, ['build.mjs'], {
    cwd: join(product, 'tools/main-diff'),
    env,
    encoding: 'utf8',
    maxBuffer: 64 << 20,
  })
  save(join(root, 'build.json'), {
    status: build.status,
    signal: build.signal,
    stdout: build.stdout,
    stderr: build.stderr,
  })
  assert.equal(build.status, 0)
  assert.equal(build.signal, null)
  const reports = {},
    commands = []
  for (const [group, selected] of groups) {
    const groupOut = join(root, group)
    mkdirSync(groupOut)
    const args = [
      join(product, 'node_modules/tsx/dist/cli.mjs'),
      join(product, 'tools/main-diff/main.ts'),
      '--group',
      group,
      '--show',
      '3',
      ...selected.flatMap((name) => ['--fn', name]),
    ]
    const run = spawnSync(process.execPath, args, {
      cwd: product,
      env: { ...env, MAIN_DIFF_OUT: groupOut },
      encoding: 'utf8',
      maxBuffer: 64 << 20,
    })
    const command = {
      group,
      functions: selected,
      status: run.status,
      signal: run.signal,
      stdout: run.stdout,
      stderr: run.stderr,
    }
    save(join(groupOut, 'command.json'), command)
    commands.push(command)
    assert.equal(run.status, 0)
    assert.equal(run.signal, null)
    reports[group] = json(join(groupOut, 'report.json'))
  }
  const value = {
    identity,
    verdict: 'success',
    functions,
    ledger,
    ledgerSha256: objectHash(ledger),
    reports,
    commands,
    sources: {
      productMainSha256: hash(checkedFile(product, 'tools/main-diff/main.ts')),
      ledgerFileSha256: hash(checkedFile(product, 'tools/main-diff/divergences.json')),
      mutatorSha256: plan.modules['tools/main-diff/faults.mjs'],
      wasmSha256: hash(readFileSync(env.SEUOFFICE_WASM)),
    },
  }
  auditRetainedBaseline(value)
  save(join(out, 'normal-baseline.json'), value)
}

export function loadRetainedEvidence(directory, plan, baselineValue) {
  assert.ok(!readdirSync(directory).includes('capture-errors.json'), 'Producer capture errors')
  assert.ok(
    lstatSync(directory).isDirectory() && !lstatSync(directory).isSymbolicLink(),
    'Invalid evidence directory',
  )
  const readRaw = (path) => {
    assert.ok(safe(path), 'Invalid evidence path')
    const parts = path.split('/')
    for (let i = 1; i <= parts.length; i++) {
      const state = lstatSync(join(directory, ...parts.slice(0, i)))
      assert.ok(!state.isSymbolicLink(), 'Symbolic evidence path')
      assert.ok(i === parts.length ? state.isFile() : state.isDirectory(), 'Invalid evidence entry')
    }
    return readFileSync(join(directory, path))
  }
  const byName = new Map(baselineValue.functions.map((f) => [f.name, f]))
  const faults = plan.faults
  const records = plan.indices.map((index) => {
    const base = `mutations/0-${index}`
    const groups = [
      ...new Set(
        faults[index].fn.map((fn) => {
          assert.ok(byName.has(fn))
          return byName.get(fn).group
        }),
      ),
    ]
    return {
      catalogOrdinal: 0,
      index,
      startRaw: readRaw(`${base}/start.json`),
      appliedRaw: readRaw(`${base}/applied.json`),
      groups: groups.map((_, position) => ({
        commandRaw: readRaw(`${base}/groups/${position}-command.json`),
        reportRaw: readRaw(`${base}/groups/${position}-report.raw.json`),
      })),
      endRaw: readRaw(`${base}/end.json`),
    }
  })
  return {
    runRaw: readRaw('run.json'),
    finishedRaw: readRaw('producer-finished.json'),
    catalogsRaw: [readRaw('catalogs/0.json')],
    records,
  }
}

async function main(command, env) {
  if (command === 'inventory-group') {
    console.log(JSON.stringify(await inventoryGroup(resolve(process.argv[3]), process.argv[4])))
    return
  }
  assert.ok(['validate', 'prepare', 'baseline', 'audit'].includes(command))
  const driver = resolve(env.DRIVER_DIR ?? 'driver'),
    product = resolve(env.PRODUCT_DIR ?? 'product')
  const manifest = json(join(driver, 'tools/main-diff/retained-sensitivity-manifest.json'))
  const identity = proofIdentity(env, manifest),
    plan = retainedPlan(manifest, identity.definitionSha)
  assert.equal(git(driver, 'rev-parse', 'HEAD').toString().trim(), identity.definitionSha)
  if (command === 'validate') {
    emit(env, 'product_sha', identity.productSha)
    return
  }
  const catalog = bindProduct(product, plan),
    planPath = resolve(env.PLAN_PATH)
  if (command === 'prepare') {
    save(planPath, plan)
    emit(env, 'product_sha', identity.productSha)
    return
  }
  assert.deepEqual(json(planPath), plan)
  assert.equal(env.MAIN_DIFF_PROFILE, 'node-faults')
  assert.equal(env.CARGO_INCREMENTAL, '1')
  assert.ok(isAbsolute(env.JOB_OUT) && isAbsolute(env.SEUOFFICE_WASM))
  const out = env.JOB_OUT
  mkdirSync(out, { recursive: true })
  if (command === 'baseline') {
    baseline(product, plan, catalog, identity, out, env)
    return
  }
  assert.equal(env.MUTATION_STATUS, 'success')
  assert.ok(isAbsolute(env.MAIN_DIFF_MUTATION_EVIDENCE_DIR))
  assert.equal(relative(out, env.MAIN_DIFF_MUTATION_EVIDENCE_DIR), 'retained')
  const value = json(join(out, 'normal-baseline.json'))
  assert.deepEqual(value.identity, identity)
  assert.deepEqual(value.functions, plan.baselineExpected)
  assert.deepEqual(value.ledger, plan.ledgerRequirements)
  assert.deepEqual(value.sources, {
    productMainSha256: hash(checkedFile(product, 'tools/main-diff/main.ts')),
    ledgerFileSha256: hash(checkedFile(product, 'tools/main-diff/divergences.json')),
    mutatorSha256: plan.modules['tools/main-diff/faults.mjs'],
    wasmSha256: hash(readFileSync(env.SEUOFFICE_WASM)),
  })
  const selectedSources = [...new Set(plan.indices.map((i) => `crates/${catalog.faults[i].file}`))]
  const sources = Object.fromEntries(
    selectedSources.map((path) => [path, checkedFile(product, path).toString('utf8')]),
  )
  const raw = loadRetainedEvidence(
    env.MAIN_DIFF_MUTATION_EVIDENCE_DIR,
    { ...plan, faults: catalog.faults },
    value,
  )
  const resultsRaw = readFileSync(join(product, 'tools/main-diff/out/faults.json'))
  const expected = {
    identity,
    modules: plan.modules,
    baselineSha256: objectHash(value),
    baselineIdentity: identity,
    baselineSources: value.sources,
    catalogs: [{ path: plan.catalog, raw: catalog.raw }],
    shard: plan.shard,
    shards: plan.shards,
    cwd: join(product, 'tools/main-diff'),
    sources,
  }
  const receipt = auditRecordedRun({
    expected,
    baseline: value,
    resultsRaw,
    ...raw,
  })
  const focus = raw.records.find((record) => record.index === plan.focus.index)
  const differences = focus.groups.flatMap((group) => JSON.parse(group.reportRaw).differences)
  const independent = differences.find(
    (d) => d.fn === plan.focus.fn && d.case === plan.focus.independentCase,
  )
  const ledger = differences.find((d) => d.fn === plan.focus.fn && d.case === plan.focus.ledgerCase)
  assert.ok(independent && ledger)
  assert.equal(independent.typescript, plan.focus.typescript)
  assert.equal(independent.rust, plan.focus.rust)
  const reportBytes = raw.records.reduce(
    (sum, record) => sum + record.groups.reduce((n, group) => n + group.reportRaw.length, 0),
    0,
  )
  save(join(out, 'retained-proof-acceptance.json'), {
    scope: plan.scope,
    identity,
    manifestSha256: plan.manifestSha256,
    planSha256: objectHash(plan),
    receipt,
    normalChecks: value.functions.reduce((n, f) => n + f.checks, 0),
    selectedIndices: plan.indices,
    focusOriginalIndex: plan.focus.index,
    laterOriginalIndex: plan.indices.at(-1),
    typedLedgerDifference: ledger,
    typedIndependentDifference: independent,
    rawReportBytes: reportBytes,
    originalRejectedEvidenceUnchanged: true,
    fullRecoveryAccepted: false,
  })
}

if (process.argv[1] && resolve(process.argv[1]) === self)
  main(process.argv[2], process.env).catch((error) => {
    console.error(error)
    process.exitCode = 1
  })
