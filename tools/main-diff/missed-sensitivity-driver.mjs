import assert from 'node:assert/strict'
import { execFileSync, spawnSync } from 'node:child_process'
import { appendFileSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve, isAbsolute, relative } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { hash, hashBytes } from './recovery-plan.mjs'
import { selectedGroups } from './recovery-normal-baseline.mjs'
import { validateSelectors } from './targeted-sensitivity.mjs'
import { runnerModel } from './recovery-runner-model.mjs'
import {
  validateManifest,
  makePlan,
  auditBaseline,
  auditTarget,
} from './missed-sensitivity-audit.mjs'

const self = fileURLToPath(import.meta.url)
const json = (path) => JSON.parse(readFileSync(path, 'utf8'))
const save = (path, value) =>
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, { flag: 'wx' })
const git = (dir, ...args) => execFileSync('git', ['-C', dir, ...args])
const emit = (env, key, value) => {
  if (env.GITHUB_OUTPUT) appendFileSync(env.GITHUB_OUTPUT, `${key}=${value}\n`)
}

export function identity(env, manifest, requireTarget = false) {
  const modes = {
    'five-miss-followup-only-not-full-recovery': 'main-missed-sensitivity',
    'ledger-consumer-followup-only-not-full-recovery': 'main-ledger-consumer-sensitivity',
    'ledger-consumer-suite-only-not-full-recovery': 'main-ledger-consumer-suite',
  }
  assert.ok(Object.hasOwn(modes, manifest.scope), 'Unknown manifest scope')
  assert.equal(env.MODE, modes[manifest.scope])
  for (const key of ['DEFINITION_SHA', 'PRODUCT_SHA'])
    assert.match(env[key] ?? '', /^[a-f0-9]{40}$/)
  assert.equal(env.PRODUCT_SHA, manifest.productSha)
  for (const key of ['GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT'])
    assert.match(env[key] ?? '', /^[1-9][0-9]*$/)
  assert.ok(Number.isSafeInteger(Number(env.GITHUB_RUN_ATTEMPT)))
  if (requireTarget || env.TARGET_ID)
    assert.ok(
      manifest.cases.some((item) => item.id === env.TARGET_ID),
      'Missing or unknown target',
    )
  return {
    definitionSha: env.DEFINITION_SHA,
    productSha: env.PRODUCT_SHA,
    runId: env.GITHUB_RUN_ID,
    attempt: Number(env.GITHUB_RUN_ATTEMPT),
  }
}

export function terminalReportPath(product, env) {
  assert.ok(env.MAIN_DIFF_OUT, 'MAIN_DIFF_OUT is required')
  return resolve(product, 'tools/main-diff', env.MAIN_DIFF_OUT, 'report.json')
}

export function validateLineage(manifest, changedPaths, hashes) {
  const entries = manifest.sourceLineage.paths
  assert.deepEqual(
    [...changedPaths].sort(),
    entries.map((entry) => entry.path).sort(),
    'Unexpected product delta',
  )
  assert.deepEqual(Object.keys(hashes).sort(), entries.map((entry) => entry.path).sort())
  for (const entry of entries)
    assert.deepEqual(
      hashes[entry.path],
      { parentSha256: entry.parentSha256, productSha256: entry.productSha256 },
      `Unexpected lineage: ${entry.path}`,
    )
}

function safePath(root, path) {
  assert.equal(typeof path, 'string')
  assert.ok(path && !isAbsolute(path) && !path.split(/[\\/]/).includes('..'))
  const dest = resolve(root, path)
  assert.ok(relative(root, dest) && !relative(root, dest).startsWith('..'))
  return dest
}

function checkedFile(product, path) {
  const bytes = readFileSync(safePath(product, path))
  assert.deepEqual(bytes, git(product, 'show', `HEAD:${path}`), `Uncommitted product file: ${path}`)
  return bytes
}

export function bindCatalogCase(item, original, faults) {
  assert.equal(original.length, item.catalogLength)
  assert.equal(faults.length, item.catalogLength)
  const previous = original[item.index]
  const current = faults[item.index]
  assert.equal(hash(previous), item.originalMutationSha256)
  assert.equal(hash(current), item.mutationSha256)
  assert.deepEqual(current, item.fault)
  if (item.mutationLineage) {
    const lineage = item.mutationLineage
    assert.deepEqual(previous, lineage.originalFault)
    assert.equal(lineage.originalSha256, item.originalMutationSha256)
    assert.equal(lineage.followupSha256, item.mutationSha256)
    assert.ok(typeof lineage.reason === 'string' && lineage.reason.length > 0)
    for (const key of ['file', 'fn', 'find']) assert.deepEqual(current[key], previous[key])
    assert.notEqual(current.name, previous.name)
    assert.notEqual(current.replace, previous.replace)
    const unchanged = (fault) =>
      Object.fromEntries(
        Object.entries(fault).filter(([key]) => key !== 'name' && key !== 'replace'),
      )
    assert.deepEqual(unchanged(current), unchanged(previous))
  } else {
    assert.deepEqual(current, previous)
  }
  assert.equal(hash(faults[0]), item.controlSha256)
  assert.deepEqual(faults[0], item.control)
  assert.deepEqual(original[0], item.control)
}

function bindProduct(product, manifest) {
  const changed = git(
    product,
    'diff',
    '--name-only',
    '-z',
    manifest.parentProductSha,
    manifest.productSha,
  )
    .toString()
    .split('\0')
    .filter(Boolean)
  const added = git(
    product,
    'diff',
    '--name-only',
    '--diff-filter=A',
    '-z',
    manifest.parentProductSha,
    manifest.productSha,
  )
    .toString()
    .split('\0')
    .filter(Boolean)
    .sort()
  assert.deepEqual(
    added,
    manifest.sourceLineage.paths
      .filter((entry) => entry.parentSha256 === null)
      .map((entry) => entry.path)
      .sort(),
    'Unexpected added lineage paths',
  )
  const hashes = Object.fromEntries(
    manifest.sourceLineage.paths.map((entry) => [
      entry.path,
      {
        parentSha256:
          entry.parentSha256 === null
            ? null
            : hashBytes(git(product, 'show', `${manifest.parentProductSha}:${entry.path}`)),
        productSha256: hashBytes(git(product, 'show', `${manifest.productSha}:${entry.path}`)),
      },
    ]),
  )
  validateLineage(manifest, changed, hashes)
  const catalogs = new Map()
  for (const item of manifest.cases) {
    const bytes = checkedFile(product, item.catalog)
    assert.equal(hashBytes(bytes), item.catalogSha256)
    const faults = JSON.parse(bytes)
    const original = JSON.parse(
      git(product, 'show', `${manifest.parentProductSha}:${item.catalog}`),
    )
    bindCatalogCase(item, original, faults)
    catalogs.set(item.catalog, { path: item.catalog, faults })
  }
  assert.equal(
    hashBytes(checkedFile(product, 'tools/main-diff/faults.mjs')),
    manifest.mutatorSha256,
  )
  return catalogs
}

function sources(product, paths, committed = false) {
  return Object.fromEntries(
    [...paths]
      .sort()
      .map((path) => [
        path,
        hashBytes(
          committed ? git(product, 'show', `HEAD:${path}`) : readFileSync(safePath(product, path)),
        ),
      ]),
  )
}

function model(product, target, catalog, before) {
  const result = runnerModel({
    job: { indices: [0, target.index] },
    catalog,
    sourceBefore: before,
    sources: Object.fromEntries(
      Object.keys(before).map((path) => [
        path,
        git(product, 'show', `HEAD:${path}`).toString('utf8'),
      ]),
    ),
    mutator: checkedFile(product, 'tools/main-diff/faults.mjs'),
  })
  return { kind: result.kind, mutatorSha256: result.mutatorSha256, entries: result.entries }
}

async function inventoryGroup(product, groupName) {
  await import(pathToFileURL(join(product, 'packages/engine-wasm/src/node.ts')))
  const { groups } = await import(pathToFileURL(join(product, 'tools/main-diff/groups/index.ts')))
  assert.ok(Object.hasOwn(groups, groupName), 'Unknown inventory group')
  const { group } = await groups[groupName]()
  return Object.entries(group).map(([name, spec]) => ({
    group: groupName,
    name,
    checks: spec.cases.length,
    single: Boolean(spec.single),
  }))
}

export function assertSelectedLedger(ledger, target) {
  assert.deepEqual(ledger, target.ledgerRequirements ?? [])
}

function collectBaseline(product, out, env, target, current, catalog) {
  for (const path of [
    'tools/main-diff/main.ts',
    'tools/main-diff/divergences.json',
    'tools/main-diff/faults.mjs',
  ])
    checkedFile(product, path)
  const list = execFileSync(
    process.execPath,
    [
      join(product, 'node_modules/tsx/dist/cli.mjs'),
      join(product, 'tools/main-diff/main.ts'),
      '--list',
    ],
    { cwd: product, env, encoding: 'utf8', maxBuffer: 64 << 20 },
  )
  validateSelectors(list, [catalog])
  const groups = selectedGroups(list, [target.control, target.fault])
  const selected = new Set(Object.values(groups).flat())
  const ledgerPath = join(product, 'tools/main-diff/divergences.json')
  const ledger = json(ledgerPath).filter((entry) => selected.has(entry.fn))
  assertSelectedLedger(ledger, target)
  const root = join(out, 'baseline')
  mkdirSync(root, { recursive: true })
  const functions = []
  for (const [group, names] of Object.entries(groups)) {
    const raw = execFileSync(
      process.execPath,
      [join(product, 'node_modules/tsx/dist/cli.mjs'), self, 'inventory-group', product, group],
      { cwd: product, env, encoding: 'utf8', maxBuffer: 64 << 20 },
    )
    const inventory = JSON.parse(raw)
    const mine = inventory.filter((entry) => names.includes(entry.name))
    assert.deepEqual(mine.map((entry) => entry.name).sort(), [...names].sort())
    functions.push(...mine)
    save(join(root, `${group}-inventory.json`), inventory)
  }
  functions.sort(
    (a, b) =>
      target.baselineExpected.findIndex((entry) => entry.name === a.name) -
      target.baselineExpected.findIndex((entry) => entry.name === b.name),
  )
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
    profile: env.MAIN_DIFF_PROFILE,
    cargoIncremental: env.CARGO_INCREMENTAL,
  })
  assert.equal(build.signal, null)
  assert.equal(build.status, 0, 'Normal build failed')
  const commands = []
  const reports = {}
  for (const [group, names] of Object.entries(groups)) {
    const groupOut = join(root, group)
    mkdirSync(groupOut)
    const run = spawnSync(
      process.execPath,
      [
        join(product, 'node_modules/tsx/dist/cli.mjs'),
        join(product, 'tools/main-diff/main.ts'),
        '--group',
        group,
        '--show',
        '3',
        ...names.flatMap((name) => ['--fn', name]),
      ],
      {
        cwd: product,
        env: { ...env, MAIN_DIFF_OUT: groupOut },
        encoding: 'utf8',
        maxBuffer: 64 << 20,
      },
    )
    const command = {
      group,
      functions: names,
      status: run.status,
      signal: run.signal,
      stdout: run.stdout,
      stderr: run.stderr,
    }
    commands.push(command)
    save(join(groupOut, 'command.json'), command)
    assert.equal(run.signal, null)
    assert.equal(run.status, 0, `Baseline failed: ${group}`)
    reports[group] = json(join(groupOut, 'report.json'))
  }
  const baseline = {
    identity: current,
    functions,
    ledger,
    ledgerSha256: hash(ledger),
    reports,
    commands,
    sources: {
      productMainSha256: hashBytes(checkedFile(product, 'tools/main-diff/main.ts')),
      ledgerFileSha256: hashBytes(checkedFile(product, 'tools/main-diff/divergences.json')),
      mutatorSha256: hashBytes(checkedFile(product, 'tools/main-diff/faults.mjs')),
      wasmSha256: hashBytes(readFileSync(env.SEUOFFICE_WASM)),
    },
    verdict: 'success',
  }
  auditBaseline(
    {
      identity: current,
      baselineExpected: target.baselineExpected,
      ledgerRequirements: target.ledgerRequirements,
    },
    baseline,
  )
  save(join(out, 'normal-baseline.json'), baseline)
  return baseline
}

async function main(command, env) {
  if (command === 'inventory-group') {
    console.log(JSON.stringify(await inventoryGroup(resolve(process.argv[3]), process.argv[4])))
    return
  }
  assert.ok(
    ['validate', 'prepare', 'job-prepare', 'audit-job'].includes(command),
    'Unknown command',
  )
  const driver = resolve(env.DRIVER_DIR ?? 'driver')
  const product = resolve(env.PRODUCT_DIR ?? 'product')
  const manifest = json(join(driver, 'tools/main-diff/missed-sensitivity-manifest.json'))
  validateManifest(manifest)
  const base = identity(env, manifest, command === 'job-prepare' || command === 'audit-job')
  assert.equal(git(driver, 'rev-parse', 'HEAD').toString().trim(), base.definitionSha)
  if (command === 'validate') {
    emit(env, 'product_sha', base.productSha)
    return
  }
  assert.equal(git(product, 'rev-parse', 'HEAD').toString().trim(), base.productSha)
  const catalogs = bindProduct(product, manifest)
  const plan = makePlan(manifest, base.definitionSha)
  const planPath = resolve(env.PLAN_PATH ?? 'missed-sensitivity-plan.json')
  if (command === 'prepare') {
    save(planPath, plan)
    emit(env, 'product_sha', base.productSha)
    emit(
      env,
      'matrix',
      JSON.stringify({ include: plan.cases.map((item) => ({ targetId: item.id })) }),
    )
    return
  }
  assert.deepEqual(json(planPath), plan, 'Downloaded plan differs from recomputed plan')
  assert.equal(env.MAIN_DIFF_PROFILE, 'node-faults')
  assert.equal(env.CARGO_INCREMENTAL, '1')
  const target = plan.cases.find((item) => item.id === env.TARGET_ID)
  assert.ok(target)
  const catalog = catalogs.get(target.catalog)
  const current = {
    ...base,
    manifestSha256: plan.manifestSha256,
    planSha256: hash(plan),
    targetId: target.id,
  }
  const out = resolve(env.JOB_OUT)
  if (command === 'job-prepare') {
    mkdirSync(out, { recursive: true })
    collectBaseline(product, out, env, target, current, catalog)
    const paths = [
      ...new Set([target.control, target.fault].map((fault) => `crates/${fault.file}`)),
    ]
    const before = sources(product, paths)
    assert.deepEqual(before, sources(product, paths, true))
    save(join(out, 'source-before.json'), before)
    save(join(out, 'job-identity.json'), current)
    save(join(out, 'source-model.json'), model(product, target, catalog, before))
    emit(env, 'fault_shard', `${target.index}/${target.modulus}`)
    emit(env, 'catalog', target.catalog)
    return
  }
  assert.equal(env.MUTATION_STATUS, 'success', 'Mutation producer did not succeed')
  assert.equal(
    terminalReportPath(product, env),
    join(out, 'mutant', 'report.json'),
    'Unexpected terminal output directory',
  )
  const before = json(join(out, 'source-before.json'))
  assert.deepEqual(before, sources(product, Object.keys(before), true))
  const after = sources(product, Object.keys(before))
  assert.deepEqual(after, before, 'Product source not restored')
  assert.deepEqual(json(join(out, 'job-identity.json')), current)
  const sourceModel = json(join(out, 'source-model.json'))
  assert.deepEqual(sourceModel, model(product, target, catalog, before))
  const baseline = json(join(out, 'normal-baseline.json'))
  assert.deepEqual(baseline.sources, {
    productMainSha256: hashBytes(checkedFile(product, 'tools/main-diff/main.ts')),
    ledgerFileSha256: hashBytes(checkedFile(product, 'tools/main-diff/divergences.json')),
    mutatorSha256: hashBytes(checkedFile(product, 'tools/main-diff/faults.mjs')),
    wasmSha256: hashBytes(readFileSync(env.SEUOFFICE_WASM)),
  })
  auditBaseline(
    {
      identity: current,
      baselineExpected: target.baselineExpected,
      ledgerRequirements: target.ledgerRequirements,
    },
    baseline,
  )
  const rowsPath = join(product, 'tools/main-diff/out/faults.json')
  copyFileSync(rowsPath, join(out, 'faults.json'))
  const rawReport = readFileSync(terminalReportPath(product, env))
  writeFileSync(join(out, 'terminal-report.json'), rawReport, { flag: 'wx' })
  const evidence = {
    identity: current,
    rows: json(rowsPath),
    text: readFileSync(join(out, 'faults.txt'), 'utf8'),
    report: JSON.parse(rawReport),
    reportSha256: hashBytes(rawReport),
    sourceBefore: before,
    sourceAfter: after,
    sourceModel,
  }
  save(join(out, 'completion.json'), auditTarget({ plan, target, baseline, evidence }))
}

if (process.argv[1] && resolve(process.argv[1]) === self)
  main(process.argv[2], process.env).catch((error) => {
    console.error(error)
    process.exitCode = 1
  })
