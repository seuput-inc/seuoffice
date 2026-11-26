// Compares two runs of apps/web/tests/bench-xlsx.spec.ts:
//   node tools/bench-xlsx-compare.mjs <dir> <before-label> <after-label> [browser]
// <dir> holds the runs' bench-xlsx-<label>-<browser>.json and screenshots
// (apps/web/test-results after a run). Prints each workbook's medians side by
// side and how many pixels of its first-cells and last-row screenshots differ.
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import pixelmatch from 'pixelmatch'
import { PNG } from 'pngjs'

const [dir, before, after, browser = 'chromium'] = process.argv.slice(2)
if (!dir || !before || !after) {
  console.error('usage: node tools/bench-xlsx-compare.mjs <dir> <before> <after> [browser]')
  process.exit(2)
}

const run = (label) =>
  JSON.parse(readFileSync(resolve(dir, `bench-xlsx-${label}-${browser}.json`), 'utf8')).results

function differentPixels(name, shot) {
  const path = (label) => resolve(dir, `bench-xlsx-${label}-${browser}-${name}-${shot}.png`)
  if (!existsSync(path(before)) || !existsSync(path(after))) return 'missing'
  const a = PNG.sync.read(readFileSync(path(before)))
  const b = PNG.sync.read(readFileSync(path(after)))
  if (a.width !== b.width || a.height !== b.height) return 'size differs'
  return pixelmatch(a.data, b.data, null, a.width, a.height, { threshold: 0.1 })
}

const a = run(before)
const b = run(after)
for (const name of Object.keys(b)) {
  if (!a[name]) continue
  console.log(`\n${name} (${browser})`)
  const metrics = new Set([...Object.keys(a[name].median), ...Object.keys(b[name].median)])
  const round = (value) => (value === undefined ? 'missing' : +value.toFixed(2))
  const rows = [...metrics].map((metric) => {
    return {
      metric,
      [before]: round(a[name].median[metric]),
      [after]: round(b[name].median[metric]),
    }
  })
  rows.push({ metric: 'pixels differing (top)', [after]: differentPixels(name, 'top') })
  rows.push({ metric: 'pixels differing (end)', [after]: differentPixels(name, 'end') })
  console.table(rows)
}
