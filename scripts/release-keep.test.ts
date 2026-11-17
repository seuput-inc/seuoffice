import { spawnSync } from 'node:child_process'
import { chmodSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'

const ROOT = join(import.meta.dirname, '..')
const SCRIPT = join(ROOT, 'scripts/release-prune.sh')
const keepList = JSON.parse(readFileSync(join(ROOT, '.github/release-keep.json'), 'utf8')) as {
  stable: number
  prerelease: number
  keep: Record<string, string>
}

describe('release keep list', () => {
  it('names vX.Y.Z tags with a reason each', () => {
    expect(keepList.stable).toBeGreaterThanOrEqual(1)
    expect(keepList.prerelease).toBeGreaterThanOrEqual(0)
    for (const [tag, reason] of Object.entries(keepList.keep)) {
      expect(tag).toMatch(/^v\d+\.\d+\.\d+$/)
      expect(reason.length).toBeGreaterThan(10)
    }
  })

  it('keeps the tags the repo still downloads or links', () => {
    const text = (p: string) => readFileSync(join(ROOT, p), 'utf8')
    expect(text('docs/seuoffice/판-내기.md') + text('packages/embed/README.md')).toContain(
      'v0.12.15',
    )
  })
})

type Rel = [tag: string, prerelease: boolean]

/** Runs the script against a fake `gh`; `latest` undefined makes the Latest lookup fail. */
function run(
  releases: Rel[],
  latest: string | undefined,
  tags: string[],
  args: string[] = [],
  env: Record<string, string> = {},
) {
  const dir = mkdtempSync(join(tmpdir(), 'prune-'))
  const log = join(dir, 'calls')
  writeFileSync(
    join(dir, 'keep.json'),
    JSON.stringify({
      stable: 3,
      prerelease: 3,
      keep: { 'v0.12.37': 'baseline', 'v0.12.15': 'docs' },
    }),
  )
  writeFileSync(join(dir, 'rows'), releases.map(([t, p]) => `${t}\t${p}`).join('\n') + '\n')
  writeFileSync(
    join(dir, 'gh'),
    `#!/usr/bin/env bash
if [ "$1" = release ]; then echo "$*" >> ${log}; exit 0; fi
case "$*" in
  *releases/latest*) ${latest === undefined ? 'exit 1' : `echo ${latest}`};;
  *git/ref/tags/*) t=$2; t=\${t##*/}; case " ${tags.join(' ')} " in *" $t "*) exit 0;; *) exit 1;; esac;;
  *releases*) cat ${join(dir, 'rows')};;
esac
`,
  )
  chmodSync(join(dir, 'gh'), 0o755)
  const r = spawnSync('bash', [SCRIPT, ...args], {
    cwd: ROOT,
    encoding: 'utf8',
    env: {
      ...process.env,
      PATH: `${dir}:${process.env.PATH}`,
      GH_REPO: 'o/r',
      KEEP_FILE: join(dir, 'keep.json'),
      ...env,
    },
  })
  let calls = ''
  try {
    calls = readFileSync(log, 'utf8')
  } catch {}
  return { status: r.status, out: r.stdout + r.stderr, calls }
}

const rel = (stable: string[], pre: string[]): Rel[] => [
  ...stable.map((t): Rel => [t, false]),
  ...pre.map((t): Rel => [t, true]),
]
const tags = ['v0.12.37', 'v0.12.15']
const deleted = (calls: string) =>
  calls
    .split('\n')
    .filter(Boolean)
    .map((l) => l.split(' ')[2])

describe('release-prune.sh', () => {
  it('orders by semantic version, deletes the rest and never removes tags', () => {
    const r = run(
      rel(
        ['v0.12.9', 'v0.12.10', 'v0.12.2', 'v0.12.1', 'v0.12.37', 'v0.12.15'],
        ['v0.12.50', 'v0.12.49', 'v0.12.48', 'v0.12.47'],
      ),
      'v0.12.10',
      tags,
    )
    expect(r.status).toBe(0)
    expect(deleted(r.calls).sort()).toEqual(['v0.12.1', 'v0.12.2', 'v0.12.47', 'v0.12.9'])
    expect(r.calls).not.toContain('--cleanup-tag')
  })

  it('keeps the stable Latest under 4 newer pre-releases', () => {
    const r = run(rel(['v0.12.1'], ['v0.12.8', 'v0.12.7', 'v0.12.6', 'v0.12.5']), 'v0.12.1', tags)
    expect(r.status).toBe(0)
    expect(deleted(r.calls)).toEqual(['v0.12.5'])
    expect(r.out).toContain('keep v0.12.1: Latest')
  })

  it('treats the previous Latest by the normal rules after a promotion', () => {
    const stables = ['v0.12.20', 'v0.12.19', 'v0.12.18', 'v0.12.17']
    const r = run(rel(stables, []), 'v0.12.20', tags)
    expect(deleted(r.calls)).toEqual(['v0.12.17'])
    const promoted = run(rel(['v0.12.21', ...stables], []), 'v0.12.21', tags)
    expect(deleted(promoted.calls).sort()).toEqual(['v0.12.17', 'v0.12.18'].sort())
  })

  it('dry run deletes nothing and prints the plan', () => {
    const r = run(rel(['v0.12.1', 'v0.12.2', 'v0.12.3', 'v0.12.4'], []), 'v0.12.4', tags, [
      '--dry-run',
    ])
    expect(r.status).toBe(0)
    expect(r.out).toContain('Latest = v0.12.4')
    expect(r.out).toContain('would delete v0.12.1')
    expect(r.calls).toBe('')
  })

  it('stops when Latest cannot be read', () => {
    const r = run(rel(['v0.12.4', 'v0.12.3'], []), undefined, tags)
    expect(r.status).not.toBe(0)
    expect(r.calls).toBe('')
  })

  it('fails when a keep tag is not a tag', () => {
    const r = run(rel(['v0.12.4'], []), 'v0.12.4', ['v0.12.37'])
    expect(r.status).not.toBe(0)
    expect(r.calls).toBe('')
  })

  it('stops when too many would be deleted', () => {
    const r = run(
      rel(['v0.12.9', 'v0.12.8', 'v0.12.7', 'v0.12.6', 'v0.12.5'], []),
      'v0.12.9',
      tags,
      [],
      { MAX_DELETE: '1' },
    )
    expect(r.status).not.toBe(0)
    expect(r.calls).toBe('')
  })
})
