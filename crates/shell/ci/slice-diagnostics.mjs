#!/usr/bin/env node
// Counts the compiler diagnostics `cargo check --message-format=json` printed, per file,
// and fails when a file in the owned set carries any.
//
//   node crates/shell/ci/slice-diagnostics.mjs diag.json \
//     --owned crates/shell/src/native/,crates/shell/src/main.rs [--cargo-status 101] [--warnings]
//
// The crate cannot compile green until every migration slice landed, so errors in files a
// slice does not own are reported but do not fail the gate. An error in an owned file may be
// hidden by an earlier error elsewhere (rustc stops a phase), so a green gate is a slice's
// claim, never the whole app's.
//
// Paths are normalised to the repository root: cargo runs in crates/shell, so a primary span
// of `src/main.rs` is `crates/shell/src/main.rs` and `../main-core/src/x.rs` is
// `crates/main-core/src/x.rs`.

import { appendFileSync, readFileSync } from 'node:fs'
import { posix } from 'node:path'

const SHELL_DIR = 'crates/shell'

function parseArguments(argv) {
  const options = { file: null, owned: [], cargoStatus: null, warnings: false }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--owned') {
      options.owned = (argv[(index += 1)] ?? '')
        .split(',')
        .map((entry) => entry.trim())
        .filter(Boolean)
    } else if (argument === '--cargo-status') {
      options.cargoStatus = Number(argv[(index += 1)])
    } else if (argument === '--warnings') {
      options.warnings = true
    } else if (!options.file && !argument.startsWith('--')) {
      options.file = argument
    } else {
      throw new Error(`unknown argument ${argument}`)
    }
  }
  if (!options.file) throw new Error('usage: slice-diagnostics.mjs <diag.json> --owned <prefixes>')
  return options
}

/** Repository-relative path of a span's file; generated spans keep their marker. */
export function normalise(fileName) {
  if (fileName.startsWith('<')) return '(generated)'
  const slashed = fileName.replaceAll('\\', '/')
  if (posix.isAbsolute(slashed) || /^[A-Za-z]:\//.test(slashed)) {
    const marker = slashed.lastIndexOf('/crates/')
    return marker === -1 ? slashed : slashed.slice(marker + 1)
  }
  return posix.normalize(posix.join(SHELL_DIR, slashed))
}

export function isOwned(path, owned) {
  return owned.some((prefix) => path.startsWith(prefix))
}

/** Diagnostics of one `cargo --message-format=json` stream. */
export function collect(text) {
  const diagnostics = []
  for (const line of text.split('\n')) {
    if (!line.startsWith('{')) continue
    let record
    try {
      record = JSON.parse(line)
    } catch {
      continue
    }
    if (record.reason !== 'compiler-message') continue
    const message = record.message
    if (!message || (message.level !== 'error' && message.level !== 'warning')) continue
    if (/^aborting due to|^could not compile|^\d+ warnings? emitted/.test(message.message)) continue
    const primary = (message.spans ?? []).find((span) => span.is_primary) ?? message.spans?.[0]
    diagnostics.push({
      level: message.level,
      code: message.code?.code ?? '',
      text: message.message,
      file: primary ? normalise(primary.file_name) : '(no span)',
      line: primary?.line_start ?? 0,
    })
  }
  return diagnostics
}

function main() {
  const options = parseArguments(process.argv.slice(2))
  const diagnostics = collect(readFileSync(options.file, 'utf8'))
  const counted = diagnostics.filter((entry) => options.warnings || entry.level === 'error')
  const perFile = new Map()
  for (const entry of diagnostics) {
    const row = perFile.get(entry.file) ?? { errors: 0, warnings: 0 }
    row[entry.level === 'error' ? 'errors' : 'warnings'] += 1
    perFile.set(entry.file, row)
  }

  const lines = ['| file | errors | warnings | owned |', '|---|---:|---:|:-:|']
  for (const [file, row] of [...perFile].sort(
    (left, right) => right[1].errors - left[1].errors || left[0].localeCompare(right[0]),
  )) {
    lines.push(
      `| ${file} | ${row.errors} | ${row.warnings} | ${isOwned(file, options.owned) ? 'yes' : ''} |`,
    )
  }
  const owned = counted.filter((entry) => isOwned(entry.file, options.owned))
  const errors = diagnostics.filter((entry) => entry.level === 'error').length
  lines.push(
    '',
    `errors: ${errors}, owned diagnostics counted: ${owned.length} (${options.warnings ? 'errors and warnings' : 'errors'})`,
  )
  const report = lines.join('\n')
  console.log(report)
  if (process.env.GITHUB_STEP_SUMMARY)
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${report}\n`)

  for (const entry of owned.slice(0, 40)) {
    console.log(
      `owned ${entry.level}${entry.code ? `[${entry.code}]` : ''} ${entry.file}:${entry.line}: ${entry.text}`,
    )
  }

  // cargo failed without a single compiler error: the lock, the manifest or a build script,
  // not a slice's code. Say so instead of passing silently.
  if (options.cargoStatus !== null && options.cargoStatus !== 0 && errors === 0) {
    console.error(
      `cargo exited ${options.cargoStatus} without compiler errors: check the lock, the manifest and build scripts (see check.stderr).`,
    )
    process.exit(2)
  }
  // An error in a crate the shell depends on stops cargo before it compiles the shell, so
  // no owned file was checked at all: that is not a pass.
  const upstream = diagnostics.filter(
    (entry) => entry.level === 'error' && !entry.file.startsWith(`${SHELL_DIR}/`),
  )
  if (options.cargoStatus !== null && options.cargoStatus !== 0 && upstream.length > 0) {
    console.error(
      `the shell was not compiled: ${upstream.length} error(s) in its dependencies, first ${upstream[0].file}:${upstream[0].line}`,
    )
    process.exit(3)
  }
  if (owned.length > 0) process.exit(1)
}

if (
  import.meta.url === `file://${process.argv[1]}` ||
  process.argv[1]?.endsWith('slice-diagnostics.mjs')
)
  main()
