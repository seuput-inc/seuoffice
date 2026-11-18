// Merges the synthetic native-contract receipts the Rust tests wrote
// (shell_contract::native_evidence, one `*.receipt.json` per passed test) into
// the array `npm --prefix tools/ipc-diff run gate -- --native-evidence <file>` reads.
//
//   node tools/ipc-diff/native-evidence.mjs <receipts folder> <merged.json>
import { readdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

const [dir, out] = process.argv.slice(2)
if (!dir || !out) {
  console.error('usage: node tools/ipc-diff/native-evidence.mjs <receipts folder> <merged.json>')
  process.exit(2)
}
const receipts = readdirSync(dir)
  .filter((name) => name.endsWith('.receipt.json'))
  .sort()
  .map((name) => JSON.parse(readFileSync(join(dir, name), 'utf8')))
writeFileSync(out, `${JSON.stringify(receipts, null, 2)}\n`)
const byChannel = new Map()
for (const r of receipts) {
  const seen = byChannel.get(r.channel) ?? new Set()
  for (const s of r.scenarios) seen.add(s)
  byChannel.set(r.channel, seen)
}
for (const [channel, scenarios] of [...byChannel].sort())
  console.log(`${channel}: ${[...scenarios].sort().join(', ')}`)
console.log(`${receipts.length} receipts -> ${out}`)
