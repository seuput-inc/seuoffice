import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

import { KNOWN_DEFECT_NOTE, decideQuit } from './quit-policy.mjs'

const script = fileURLToPath(new URL('quit-policy.mjs', import.meta.url))

describe('an app that quits on the first request', () => {
  it.each(['0.12.42', '0.12.43', '0.12.44', '1.0.0'])('%s passes with no note', (version) => {
    expect(decideQuit({ version, exitedAfterFirst: true })).toEqual({
      action: 'pass',
    })
  })
})

describe('0.12.43 and older, which leave a Word-tab quit hanging', () => {
  it.each(['0.12.43', '0.12.42', '0.12.1', '0.11.99', '0.0.1'])(
    '%s may be asked once more',
    (version) => {
      expect(decideQuit({ version, exitedAfterFirst: false })).toEqual({
        action: 'send-second-request',
      })
    },
  )

  it('is recorded with the known-defect note when the second request ends it', () => {
    const verdict = decideQuit({
      version: '0.12.42',
      exitedAfterFirst: false,
      secondSent: true,
      exitedAfterSecond: true,
    })
    expect(verdict).toEqual({ action: 'pass', note: KNOWN_DEFECT_NOTE })
    expect(KNOWN_DEFECT_NOTE).toBe(
      'known old-version quit defect (Word tab open at quit); ended by a second request',
    )
  })

  it('fails when a second request does not end it either', () => {
    const verdict = decideQuit({
      version: '0.12.43',
      exitedAfterFirst: false,
      secondSent: true,
      exitedAfterSecond: false,
    })
    expect(verdict.action).toBe('fail')
    expect(verdict.note).toBeUndefined()
  })
})

describe('0.12.44 and newer', () => {
  it.each(['0.12.44', '0.12.45', '0.12.100', '0.13.0', '1.0.0'])(
    '%s fails on the first request that does not end it, with no second request',
    (version) => {
      const verdict = decideQuit({ version, exitedAfterFirst: false })
      expect(verdict.action).toBe('fail')
      expect(verdict.reason).toContain('no second request')
    },
  )

  it('never accepts an exit that needed a second request', () => {
    const verdict = decideQuit({
      version: '0.12.44',
      exitedAfterFirst: false,
      secondSent: true,
      exitedAfterSecond: true,
    })
    expect(verdict.action).toBe('fail')
    expect(verdict.note).toBeUndefined()
  })
})

describe('input it cannot read fails closed', () => {
  it.each([
    undefined,
    null,
    '',
    'v0.12.43',
    '0.12',
    '0.12.43.1',
    '0.12.43-beta',
    '0.12.x',
    '0.12.043',
    ' 0.12.43',
    12,
  ])('the version %j', (version) => {
    for (const exitedAfterFirst of [true, false]) {
      expect(decideQuit({ version, exitedAfterFirst }).action).toBe('fail')
    }
  })

  it.each([undefined, null, 'yes', 1])('the exit flag %j', (flag) => {
    expect(decideQuit({ version: '0.12.42', exitedAfterFirst: flag }).action).toBe('fail')
    expect(
      decideQuit({
        version: '0.12.42',
        exitedAfterFirst: false,
        secondSent: true,
        exitedAfterSecond: flag,
      }).action,
    ).toBe('fail')
  })

  it('a second request recorded for an app that had already quit', () => {
    expect(
      decideQuit({
        version: '0.12.42',
        exitedAfterFirst: true,
        secondSent: true,
      }).action,
    ).toBe('fail')
  })
})

describe('the command the Mac script runs', () => {
  const run = (...args) => {
    const done = spawnSync(process.execPath, [script, ...args], {
      encoding: 'utf8',
    })
    return { status: done.status, verdict: JSON.parse(done.stdout) }
  }

  it('exits 0 for a quit on the first request', () => {
    expect(run('0.12.44', 'yes')).toEqual({
      status: 0,
      verdict: { action: 'pass' },
    })
  })

  it('exits 10 to ask an old version once more, then 0 with the note', () => {
    expect(run('0.12.42', 'no').status).toBe(10)
    expect(run('0.12.42', 'no', 'yes', 'yes')).toEqual({
      status: 0,
      verdict: { action: 'pass', note: KNOWN_DEFECT_NOTE },
    })
  })

  it('exits 1 for a new version that did not quit, and for words it does not know', () => {
    expect(run('0.12.44', 'no').status).toBe(1)
    expect(run('0.12.42', 'maybe').status).toBe(1)
    expect(run('0.12.42', 'no', 'yes', 'perhaps').status).toBe(1)
    expect(run('not-a-version', 'yes').status).toBe(1)
  })
})
