// Fault injection for the central process comparison (decision 0030): each
// fault of a file changes one place of the Rust, the addon is rebuilt, and the
// comparison of the fault's functions must then report a difference.
//
//   node tools/main-diff/faults.mjs tools/main-diff/faults/b1.json[,more.json...]
//
// A faults file is a JSON array of
//   { "name": "...", "file": "<path under crates/>", "find": "<text>",
//     "replace": "<text>", "fn": ["<function>", ...] }
// `find` must occur exactly once in the file. The first entry of each file is
// the positive control: a fault known to show. When it is missed the run stops, because a
// comparison that sees nothing proves nothing. Each fault is undone before the
// next, also when the build fails.
//
// Verdicts: CAUGHT (an unlisted difference), MISSED (the comparison passed:
// name an input that would show it), BUILD (the faulted Rust does not build:
// not a fault, rewrite it), BAD (find is not there exactly once).
import { execFileSync, spawnSync } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createMutationEvidence, runComparison } from './mutation-evidence.mjs'

const tool = dirname(fileURLToPath(import.meta.url))
const crates = resolve(tool, '../../crates')
const files = process.argv[2].split(',').filter(Boolean)
// FAULT_SHARD=i/n runs the entries whose index is i modulo n, and each file's
// positive control in every shard
const [shard, shards] = (process.env.FAULT_SHARD ?? '0/1').split('/').map(Number)
// each fault rebuilds the addon: unoptimized and incremental, unless asked otherwise
process.env.MAIN_DIFF_PROFILE ??= 'node-faults'
const results = []
const evidence =
  process.env.MAIN_DIFF_MUTATION_EVIDENCE_DIR === undefined
    ? null
    : createMutationEvidence({
        directory: process.env.MAIN_DIFF_MUTATION_EVIDENCE_DIR,
        root: resolve(tool, '../..'),
        files,
        shard,
        shards,
      })

// the group of each function: a comparison runs only the groups a fault names
const listed = spawnSync('npx', ['tsx', 'main.ts', '--list'], { cwd: tool, encoding: 'utf8' })
if (listed.status !== 0) {
  console.error(
    `main.ts --list failed (${listed.status ?? listed.signal}):\n${listed.stdout}${listed.stderr}`,
  )
  process.exit(1)
}
const groupOf = new Map(
  listed.stdout
    .split('\n')
    .filter(Boolean)
    .map((line) => line.split('\t').reverse()),
)

function compare(fns, mutation) {
  let status = 0
  let out = ''
  let position = 0
  for (const group of new Set(fns.map((f) => groupOf.get(f) ?? 'unknown'))) {
    const mine = fns.filter((f) => (groupOf.get(f) ?? 'unknown') === group)
    const args = [
      'tsx',
      'main.ts',
      '--group',
      group,
      '--show',
      '3',
      ...mine.flatMap((f) => ['--fn', f]),
    ]
    const run = runComparison({
      evidence,
      mutation,
      position,
      group,
      functions: mine,
      command: 'npx',
      args,
      cwd: tool,
    })
    if (run === null) return null
    position++
    status ||= run.status
    out += `${run.stdout}${run.stderr}`
  }
  return { status, out }
}

catalogLoop: for (const [ordinal, file] of files.entries()) {
  const catalogBytes = evidence ? readFileSync(resolve(file)) : null
  const faults = JSON.parse(
    catalogBytes ? catalogBytes.toString('utf8') : readFileSync(resolve(file), 'utf8'),
  )
  const catalog = evidence?.catalog(ordinal, file, catalogBytes, faults.length)
  console.log(`\n${file}`)
  for (const [i, fault] of faults.entries()) {
    if (i !== 0 && i % shards !== shard) continue
    const path = join(crates, fault.file)
    // a file that is gone (moved, merged) is a BAD entry, not the end of the shard
    const exists = existsSync(path)
    const original = exists ? readFileSync(path, 'utf8') : ''
    const count = original.split(fault.find).length - 1
    const mutation = evidence?.begin(catalog, i, fault, path, original, exists, count)
    let verdict
    let detail
    let captureFailed = false
    if (count !== 1) {
      verdict = 'BAD'
      detail = `find occurs ${count} times`
    } else {
      writeFileSync(path, original.replace(fault.find, fault.replace))
      evidence?.applied(mutation)
      try {
        execFileSync('node', ['build.mjs'], { cwd: tool, stdio: 'pipe' })
        const comparison = compare(fault.fn, mutation)
        if (comparison === null) {
          captureFailed = true
        } else {
          const { status, out } = comparison
          verdict = status === 0 ? 'MISSED' : /^DIFF /m.test(out) ? 'CAUGHT' : 'MISSED'
          detail = out
            .split('\n')
            .filter(
              (l) =>
                l.startsWith('DIFF ') ||
                / checks, \d+ equal/.test(l) ||
                /FLOOR|STALE|Error/.test(l),
            )
            .slice(0, 4)
            .join(' | ')
        }
      } catch (err) {
        verdict = 'BUILD'
        detail = String(err.stderr ?? err)
          .split('\n')
          .filter((l) => /^error/.test(l))
          .slice(0, 2)
          .join(' | ')
      } finally {
        writeFileSync(path, original)
      }
    }
    if (captureFailed) break catalogLoop
    evidence?.end(mutation, verdict, detail)
    results.push({
      faults: file,
      name: fault.name,
      file: fault.file,
      fn: fault.fn,
      verdict,
      detail,
    })
    console.log(`${verdict.padEnd(6)} ${fault.name}${detail ? `  (${detail})` : ''}`)
    if (i === 0 && verdict !== 'CAUGHT') {
      console.log('the positive control was not caught: skipping the rest of this file')
      process.exitCode = 1
      break
    }
  }
}

const out = join(tool, 'out')
mkdirSync(out, { recursive: true })
const resultsBytes = `${JSON.stringify(results, null, 2)}\n`
writeFileSync(join(out, 'faults.json'), resultsBytes)
const count = (v) => results.filter((r) => r.verdict === v).length
console.log(
  `\n${results.length} faults: ${count('CAUGHT')} caught, ${count('MISSED')} missed, ${count('BUILD')} not building, ${count('BAD')} bad`,
)
// rebuild the unfaulted addon, so a later step compares the code as it is
execFileSync('node', ['build.mjs'], { cwd: tool, stdio: 'inherit' })
if (evidence && !evidence.finish(resultsBytes)) process.exitCode = 1
