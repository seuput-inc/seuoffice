// What the migration test may do when the app it asked to quit does not quit:
// Mac builds up to 0.12.43 left the process running after a quit while a Word tab
// was open (the shell window's close prompt called the quit off, and macOS does
// not quit on its last window closing); one more quit request ends them. From
// 0.12.44 the first request must be enough.
//
//   node quit-policy.mjs <quitting version> <first: yes|no> [<second sent: yes|no> <second: yes|no>]
//
// first: the app was gone within the first-request window. second sent / second: one
// more request was sent, and the app was gone within the same window after it.
// Prints one JSON line {action, note?, reason?}. Exit codes: 0 pass, 10 send one
// more quit request now, 1 fail (also for an input it cannot read).

import { fileURLToPath } from 'node:url'

/** the newest version that has the defect */
const LAST_DEFECTIVE = [0, 12, 43]

export const KNOWN_DEFECT_NOTE =
  'known old-version quit defect (Word tab open at quit); ended by a second request'

/** [major, minor, patch] of a plain x.y.z version (no v, no suffix, no leading zeros), else null */
export function parseVersion(text) {
  if (typeof text !== 'string') return null
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(text)
  return match ? match.slice(1).map(Number) : null
}

function hasKnownDefect(version) {
  for (let i = 0; i < 3; i++) {
    if (version[i] !== LAST_DEFECTIVE[i]) return version[i] < LAST_DEFECTIVE[i]
  }
  return true
}

const fail = (reason) => ({ action: 'fail', reason })

/**
 * @param {{ version: unknown, exitedAfterFirst: unknown, secondSent?: unknown, exitedAfterSecond?: unknown }} state
 * @returns {{ action: 'pass' | 'send-second-request' | 'fail', note?: string, reason?: string }}
 */
export function decideQuit({
  version,
  exitedAfterFirst,
  secondSent = false,
  exitedAfterSecond = false,
}) {
  const parsed = parseVersion(version)
  if (!parsed) return fail(`cannot read the quitting version ${JSON.stringify(version)}`)
  for (const flag of [exitedAfterFirst, secondSent, exitedAfterSecond]) {
    if (typeof flag !== 'boolean') return fail('the quit state is not yes or no')
  }
  if (exitedAfterFirst) {
    return secondSent
      ? fail('a second request was sent to an app that had quit')
      : { action: 'pass' }
  }
  if (!hasKnownDefect(parsed)) {
    return fail(
      secondSent
        ? `${version} must quit on the first request; no second request is allowed`
        : `${version} did not quit on the first request, and from 0.12.44 no second request is allowed`,
    )
  }
  if (!secondSent) return { action: 'send-second-request' }
  return exitedAfterSecond
    ? { action: 'pass', note: KNOWN_DEFECT_NOTE }
    : fail(`${version} did not quit after a second request either`)
}

const yesNo = (text) => (text === 'yes' ? true : text === 'no' ? false : undefined)

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [version, first, secondSent, second] = process.argv.slice(2)
  const verdict = decideQuit({
    version,
    exitedAfterFirst: yesNo(first),
    ...(secondSent === undefined ? {} : { secondSent: yesNo(secondSent) }),
    ...(second === undefined ? {} : { exitedAfterSecond: yesNo(second) }),
  })
  console.log(JSON.stringify(verdict))
  process.exit({ pass: 0, 'send-second-request': 10, fail: 1 }[verdict.action])
}
