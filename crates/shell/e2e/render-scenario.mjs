#!/usr/bin/env node
// Writes a scenario with its ${FIXTURES} and ${WORK} placeholders replaced, so the file the app
// runs holds absolute paths and the checker reads exactly that file back.
//
//   node crates/shell/e2e/render-scenario.mjs <scenario.json> <out.json> <fixtures dir> <work dir>
//
// The directories are written with forward slashes (a Windows path as C:/a/b is valid JSON and
// valid for the Win32 file APIs), whatever separator they came in with.

import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { resolve } from 'node:path'

export function render(value, replacements) {
  if (typeof value === 'string') {
    return Object.entries(replacements).reduce(
      (text, [name, replacement]) => text.replaceAll('${' + name + '}', replacement),
      value,
    )
  }
  if (Array.isArray(value)) return value.map((item) => render(item, replacements))
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, render(item, replacements)]),
    )
  }
  return value
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [input, output, fixtures, work] = process.argv.slice(2)
  if (!input || !output || !fixtures || !work) {
    console.error('usage: render-scenario.mjs <scenario.json> <out.json> <fixtures dir> <work dir>')
    process.exit(64)
  }
  const slash = (dir) => dir.replaceAll('\\', '/').replace(/\/+$/, '')
  const scenario = JSON.parse(readFileSync(input, 'utf8'))
  const rendered = render(scenario, { FIXTURES: slash(fixtures), WORK: slash(work) })
  writeFileSync(output, `${JSON.stringify(rendered, null, 2)}\n`)
}
