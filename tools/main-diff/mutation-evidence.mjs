import { createHash } from 'node:crypto'
import { execFileSync, spawnSync } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, isAbsolute, join, resolve } from 'node:path'

const hash = (bytes) => createHash('sha256').update(bytes).digest('hex')
const json = (value) => Buffer.from(`${JSON.stringify(value, null, 2)}\n`)
const sourceState = (path) =>
  existsSync(path)
    ? { exists: true, sha256: hash(readFileSync(path)) }
    : { exists: false, sha256: null }

export function runComparison({
  evidence,
  mutation,
  position,
  group,
  functions,
  command,
  args,
  cwd,
  env = process.env,
}) {
  if (!evidence) return spawnSync(command, args, { cwd, encoding: 'utf8' })
  const work = evidence.work(mutation, position)
  if (work === undefined) return null
  const run = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    env: { ...env, MAIN_DIFF_OUT: work },
  })
  evidence.group(mutation, position, group, functions, args, cwd, run, work)
  return run
}

export function createMutationEvidence({
  directory,
  root,
  files,
  shard,
  shards,
  env = process.env,
}) {
  if (!isAbsolute(directory)) throw new Error('MAIN_DIFF_MUTATION_EVIDENCE_DIR must be absolute')
  const head = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim()
  if (
    !/^[0-9a-f]{40}$/.test(env.PRODUCT_SHA ?? '') ||
    env.PRODUCT_SHA !== head ||
    !/^[0-9a-f]{40}$/.test(env.DEFINITION_SHA ?? '') ||
    !/^[1-9][0-9]*$/.test(env.GITHUB_RUN_ID ?? '') ||
    !/^[1-9][0-9]*$/.test(env.GITHUB_RUN_ATTEMPT ?? '') ||
    !Number.isSafeInteger(Number(env.GITHUB_RUN_ATTEMPT))
  ) {
    throw new Error('invalid mutation evidence identity')
  }
  mkdirSync(directory)
  const errors = []
  const catalogs = []
  const records = []
  const publish = (path, bytes) => {
    mkdirSync(dirname(path), { recursive: true })
    writeFileSync(path, bytes, { flag: 'wx' })
    return hash(bytes)
  }
  const capture = (label, action) => {
    try {
      return action()
    } catch (error) {
      errors.push({ operation: label, error: String(error) })
      return null
    }
  }
  const modules = Object.fromEntries(
    ['tools/main-diff/faults.mjs', 'tools/main-diff/mutation-evidence.mjs'].map((path) => [
      path,
      hash(readFileSync(resolve(root, path))),
    ]),
  )
  const runSha256 = publish(
    join(directory, 'run.json'),
    json({
      contract: 'per-mutation-producer-evidence/v1',
      identity: {
        productSha: env.PRODUCT_SHA,
        definitionSha: env.DEFINITION_SHA,
        runId: env.GITHUB_RUN_ID,
        attempt: Number(env.GITHUB_RUN_ATTEMPT),
      },
      producer: { modules },
      buildEnvironment: { profile: env.MAIN_DIFF_PROFILE, cargoIncremental: env.CARGO_INCREMENTAL },
      catalogPaths: files,
      shard,
      shards,
    }),
  )
  return {
    catalog(ordinal, path, bytes, length) {
      const catalog = { ordinal, path, sha256: hash(bytes), length }
      capture('catalog', () =>
        publish(join(directory, 'catalogs', `${ordinal}.json`), json(catalog)),
      )
      catalogs.push({ ordinal, sha256: catalog.sha256 })
      return catalog
    },
    begin(catalog, index, fault, path, original, exists, count) {
      const base = join(directory, 'mutations', `${catalog.ordinal}-${index}`)
      const beforeErrors = errors.length
      const start = {
        catalog,
        index,
        mutationSha256: hash(JSON.stringify(fault)),
        fault,
        isControl: index === 0,
        source: {
          path: `crates/${fault.file}`,
          exists,
          beforeSha256: exists ? hash(Buffer.from(original)) : null,
          findCount: count,
          derivedAppliedSourceSha256:
            count === 1 ? hash(Buffer.from(original.replace(fault.find, fault.replace))) : null,
        },
      }
      const startSha256 = capture('start', () => {
        mkdirSync(join(directory, 'mutations'), { recursive: true })
        mkdirSync(base)
        return publish(join(base, 'start.json'), json(start))
      })
      const state = {
        base,
        catalog,
        index,
        path,
        start,
        startSha256,
        appliedSha256: null,
        groups: [],
        beforeErrors,
      }
      return state
    },
    applied(state) {
      capture('applied', () => {
        const observedSha256 = hash(readFileSync(state.path))
        if (observedSha256 !== state.start.source.derivedAppliedSourceSha256)
          throw new Error('applied source mismatch')
        state.appliedSha256 = publish(
          join(state.base, 'applied.json'),
          json({ path: state.start.source.path, observedSha256 }),
        )
      })
    },
    work(state, position) {
      const path = join(
        directory,
        'work',
        `${state.catalog.ordinal}-${state.index}`,
        String(position),
      )
      const ready = capture('work', () => {
        mkdirSync(join(directory, 'work', `${state.catalog.ordinal}-${state.index}`), {
          recursive: true,
        })
        mkdirSync(path)
        return true
      })
      return ready ? path : undefined
    },
    group(state, position, group, functions, args, cwd, run, work) {
      capture('group', () => {
        const reportPath = work === undefined ? null : join(work, 'report.json')
        let reportSha256 = null
        let raw = null
        if (reportPath !== null && existsSync(reportPath)) {
          raw = readFileSync(reportPath)
          reportSha256 = hash(raw)
          publish(join(state.base, 'groups', `${position}-report.raw.json`), raw)
        }
        const sourceSha256AtCompare = hash(readFileSync(state.path))
        const command = {
          position,
          group,
          functions,
          command: 'npx',
          args,
          cwd,
          status: run.status,
          signal: run.signal,
          stdout: run.stdout,
          stderr: run.stderr,
          reportSha256,
          sourceSha256AtCompare,
        }
        const commandSha256 = publish(
          join(state.base, 'groups', `${position}-command.json`),
          json(command),
        )
        state.groups.push({ position, commandSha256, reportSha256 })
        if (work === undefined) throw new Error('comparison work directory unavailable')
        if (raw === null) throw new Error('comparison report missing')
        JSON.parse(raw.toString('utf8'))
      })
    },
    end(state, verdict, detail) {
      capture('end', () => {
        const sourceAfter = sourceState(state.path)
        const restored =
          sourceAfter.exists === state.start.source.exists &&
          sourceAfter.sha256 === state.start.source.beforeSha256
        if (!restored) throw new Error('source restoration mismatch')
        if (errors.length !== state.beforeErrors || !state.startSha256)
          throw new Error('mutation capture incomplete')
        const endSha256 = publish(
          join(state.base, 'end.json'),
          json({
            startSha256: state.startSha256,
            appliedSha256: state.appliedSha256,
            groups: state.groups,
            verdict,
            detail,
            sourceAfter,
            restored: true,
          }),
        )
        records.push({
          catalogOrdinal: state.catalog.ordinal,
          index: state.index,
          startSha256: state.startSha256,
          endSha256,
        })
      })
    },
    finish(resultsBytes) {
      if (errors.length === 0)
        capture('finish', () =>
          publish(
            join(directory, 'producer-finished.json'),
            json({
              runSha256,
              catalogs,
              records,
              resultsSha256: hash(resultsBytes),
              finalBuildCompleted: true,
            }),
          ),
        )
      if (errors.length) {
        capture('capture-errors', () =>
          publish(join(directory, 'capture-errors.json'), json(errors)),
        )
        for (const error of errors)
          console.error(`mutation evidence ${error.operation}: ${error.error}`)
        return false
      }
      return true
    },
  }
}
