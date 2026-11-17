// Reads the targets of a pushed migration-test/* branch (a dispatch needs the
// workflow on main, a push does not) and prints them as GITHUB_OUTPUT lines.
//
//   node targets.mjs [targets.json]
//
// {"from": ["v0.12.42"], "to_tag": "v0.12.43", "next_tag": "", "faults": ["none"]}
// from: Electron release tags, or "oldest" / "previous"; to_tag / next_tag:
// existing release tags, pre-releases included; faults: Windows faults to run.

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

const TAG = /^v\d+\.\d+\.\d+$/
const FAULTS = ['none', 'kill-installer', 'block-start', 'lock-file']

export function parseTargets(text) {
  const t = JSON.parse(text)
  const from = t.from ?? []
  if (!Array.isArray(from) || !from.every((f) => f === 'oldest' || f === 'previous' || TAG.test(f))) {
    throw new Error('from must be a list of vX.Y.Z tags, "oldest" or "previous"')
  }
  for (const key of ['to_tag', 'next_tag']) {
    if (t[key] && !TAG.test(t[key])) throw new Error(`${key} must be a vX.Y.Z tag`)
  }
  if (t.next_tag && !t.to_tag) throw new Error('next_tag needs to_tag')
  const faults = t.faults ?? []
  if (!Array.isArray(faults) || !faults.every((f) => FAULTS.includes(f))) {
    throw new Error(`faults must be a list of ${FAULTS.join(', ')}`)
  }
  return {
    from: from.join(','),
    to_tag: t.to_tag ?? '',
    next_tag: t.next_tag ?? '',
    faults: faults.join(','),
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const file = process.argv[2] ?? fileURLToPath(new URL('targets.json', import.meta.url))
  for (const [key, value] of Object.entries(parseTargets(readFileSync(file, 'utf8')))) {
    console.log(`${key}=${value}`)
  }
}
