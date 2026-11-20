// Child of e2e-disk.mjs: reads one file with the repo's own parser (packages/file-parse, the wasm
// engine behind it) and prints one JSON line, `{ ok, text }` or `{ ok, error }`. Run as
//
//   node --conditions=seuoffice-source --import tsx crates/shell/ci/e2e-engine-text.mjs <file>
//
// from the repository root, after `npm ci` and `npm run build:wasm`. `unavailable` marks a failure
// of the tooling itself (a missing build), as opposed to a parser that read the file and refused it.

const file = process.argv[2]

function print(result) {
  process.stdout.write(`${JSON.stringify(result)}\n`)
}

let parse
try {
  ;({ parseFileToText: parse } = await import('@seuoffice/file-parse'))
} catch (error) {
  print({
    ok: false,
    unavailable: true,
    error: `cannot load @seuoffice/file-parse: ${error.message}`,
  })
  process.exit(0)
}
try {
  // the whole file: a marker in a far row of a large workbook is past the default text budget
  const parsed = await parse(file, { maxChars: 100_000_000 })
  print(
    parsed.ok
      ? { ok: true, text: parsed.text ?? '' }
      : { ok: false, error: parsed.error ?? 'refused' },
  )
} catch (error) {
  print({ ok: false, error: error instanceof Error ? error.message : String(error) })
}
process.exit(0)
