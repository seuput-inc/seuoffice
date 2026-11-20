import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { NATIVE_CHANNEL_CONTRACTS } from '../shell-inventory/native-channels.ts'
import { nativeContractCoverage, nativeEvidence } from './native-contracts.ts'
import { CHANNELS } from './channels.ts'
import {
  type Host,
  type HostReceipt,
  describe as describeHosts,
  findReport,
  loadReport,
  main,
  mergeHosts,
  mergeInto,
} from './native-host-evidence.ts'

const CHANNEL = 'diagram:recovery-initialized'
const SHA = 'a'.repeat(40)
let root: string
beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), 'native-host-evidence-'))
})
afterEach(() => {
  rmSync(root, { recursive: true, force: true })
})

/** Accounting fixtures only. They are not evidence of native execution. */
function receipt(os: Host, scenarios: string[], status: HostReceipt['status'] = 'passed') {
  return {
    channel: CHANNEL,
    kind: 'native-host' as const,
    status,
    scenarios,
    artifact: 'diagram-recovery/run-1/out/native-ipc.jsonl',
    os,
    spec: 'diagram-recovery',
  }
}

/** An artifact as downloaded: `below` is where its root put the e2e folder */
function artifact(os: Host, receipts: HostReceipt[], below = '', head = SHA): string {
  const dir = join(root, os)
  const host = join(dir, below, 'native-host')
  mkdirSync(join(host, 'diagram-recovery/run-1/out'), { recursive: true })
  writeFileSync(join(host, 'diagram-recovery/run-1/out/native-ipc.jsonl'), '')
  writeFileSync(join(host, 'receipts.json'), JSON.stringify({ os, head_sha: head, receipts }))
  return dir
}

describe('native-host receipts of an e2e run', () => {
  it('are found wherever the artifact put them and get their records resolved', () => {
    const mac = artifact('macOS', [receipt('macOS', ['successful-ack'])])
    const windows = artifact('Windows', [receipt('Windows', ['successful-ack'])], 'e2e')
    expect(findReport(mac)).toBe(join(mac, 'native-host', 'receipts.json'))
    expect(findReport(windows)).toBe(join(windows, 'e2e', 'native-host', 'receipts.json'))
    expect(findReport(join(root, 'none'))).toBeUndefined()
    const [loaded] = loadReport(windows, 'Windows', SHA)
    expect(loaded!.artifact).toBe(
      join(windows, 'e2e', 'native-host', 'diagram-recovery/run-1/out/native-ipc.jsonl'),
    )
  })

  it('are refused from another commit, another OS, or without their records', () => {
    const stale = artifact('macOS', [receipt('macOS', ['successful-ack'])], '', 'b'.repeat(40))
    expect(() => loadReport(stale, 'macOS', SHA)).toThrow(/of commit b+, not a+/)
    const swapped = artifact('Windows', [receipt('Windows', ['successful-ack'])])
    expect(() => loadReport(swapped, 'macOS', SHA)).toThrow(/receipts are of Windows/)
    const foreign = artifact('macOS', [receipt('Windows', ['successful-ack'])])
    expect(() => loadReport(foreign, 'macOS', SHA)).toThrow(/not a native-host receipt of macOS/)
    const lost = artifact('macOS', [
      { ...receipt('macOS', ['successful-ack']), artifact: 'gone/native-ipc.jsonl' },
    ])
    expect(() => loadReport(lost, 'macOS', SHA)).toThrow(/is missing/)
    expect(() => loadReport(join(root, 'none'), 'macOS', SHA)).toThrow(/no native-host/)
  })

  it('pass a scenario only where both OSes pass it, and keep every failure', () => {
    const merged = mergeHosts({
      macOS: [receipt('macOS', ['successful-ack', 'duplicate-ack', 'missing-receipt'])],
      Windows: [
        receipt('Windows', ['successful-ack', 'missing-receipt']),
        receipt('Windows', ['duplicate-ack'], 'failed'),
      ],
    })
    expect(merged.map(({ os, status, scenarios }) => ({ os, status, scenarios }))).toEqual([
      { os: 'macOS', status: 'passed', scenarios: ['successful-ack', 'missing-receipt'] },
      { os: 'Windows', status: 'passed', scenarios: ['successful-ack', 'missing-receipt'] },
      { os: 'Windows', status: 'failed', scenarios: ['duplicate-ack'] },
    ])
    expect(mergeHosts({ macOS: [receipt('macOS', ['successful-ack'])], Windows: [] })).toEqual([])
    expect(
      describeHosts({
        macOS: [receipt('macOS', ['successful-ack', 'duplicate-ack'])],
        Windows: [
          receipt('Windows', ['successful-ack']),
          receipt('Windows', ['duplicate-ack'], 'failed'),
        ],
      }),
    ).toEqual([
      `${CHANNEL}: both successful-ack; macOS only duplicate-ack; failed on Windows duplicate-ack`,
    ])
  })

  it('replace the native-host receipts of the evidence file and leave the synthetic ones', () => {
    const file = join(root, 'native-evidence.json')
    const synthetic = { ...receipt('macOS', ['successful-ack']), kind: 'synthetic' as const }
    writeFileSync(file, JSON.stringify([synthetic, receipt('macOS', ['failed-apply'])]))
    expect(mergeInto(file, [receipt('Windows', ['successful-ack'])])).toBe(1)
    const written = JSON.parse(readFileSync(file, 'utf8')) as HostReceipt[]
    expect(written.map(({ kind, scenarios }) => ({ kind, scenarios }))).toEqual([
      { kind: 'synthetic', scenarios: ['successful-ack'] },
      { kind: 'native-host', scenarios: ['successful-ack'] },
    ])
    const fresh = join(root, 'fresh.json')
    expect(mergeInto(fresh, [])).toBe(0)
    expect(JSON.parse(readFileSync(fresh, 'utf8'))).toEqual([])
  })

  it('leave the native-host track pending for a scenario one OS did not pass', () => {
    const merged = mergeHosts({
      macOS: [receipt('macOS', ['successful-ack', 'duplicate-ack'])],
      Windows: [receipt('Windows', ['successful-ack'])],
    })
    const [report] = nativeContractCoverage('D1', CHANNELS, nativeEvidence(merged)).filter(
      (entry) => entry.channel === CHANNEL,
    )
    expect(report!.nativeHost.status).toBe('pending')
    expect(report!.nativeHost.missingScenarios).toContain('duplicate-ack')
    expect(report!.nativeHost.missingScenarios).not.toContain('successful-ack')
  })

  it('are refused when one OS names a waived scenario, before the two are intersected', () => {
    // html:recovery-applied failed-apply is waived (tools/shell-inventory/native-waivers.mjs)
    const waived = 'html:recovery-applied'
    const reached = (os: Host, status: HostReceipt['status'] = 'passed') => ({
      ...receipt(os, ['failed-apply'], status),
      channel: waived,
    })
    // every other scenario of the channel, passed on both OSes
    const contract = NATIVE_CHANNEL_CONTRACTS.find((entry) => entry.name === waived)!
    const rest = contract.requiredScenarios.filter((scenario) => scenario !== 'failed-apply')
    const complete = (os: Host) => [
      receipt(os, ['successful-ack']),
      { ...receipt(os, rest), channel: waived },
    ]
    const evidence = join(root, 'native-evidence.json')
    const merge = () =>
      main([
        '--sha',
        SHA,
        '--mac',
        join(root, 'macOS'),
        '--windows',
        join(root, 'Windows'),
        '--evidence',
        evidence,
      ])
    const log = vi.spyOn(console, 'log').mockImplementation(() => {})
    const refusal = vi.spyOn(console, 'error').mockImplementation(() => {})
    try {
      artifact('macOS', complete('macOS'))
      artifact('Windows', complete('Windows'))
      expect(merge()).toBe(0)
      rmSync(evidence)

      // passed on macOS alone: the intersection would have dropped it, and the waiver with it
      for (const [os, status] of [
        ['macOS', 'passed'],
        ['Windows', 'passed'],
        ['macOS', 'failed'],
      ] as const) {
        artifact('macOS', complete('macOS'))
        artifact('Windows', complete('Windows'))
        artifact(os, [...complete(os), reached(os, status)])
        refusal.mockClear()
        expect(merge(), `${os} ${status}`).toBe(1)
        expect(refusal).toHaveBeenCalledWith(
          `native-host receipts refused: ${os}: a receipt of ${waived} names the waived scenario ` +
            'failed-apply: a spec drives it, so drop its waiver (tools/shell-inventory/native-waivers.mjs)',
        )
        expect(existsSync(evidence)).toBe(false)
      }
    } finally {
      log.mockRestore()
      refusal.mockRestore()
    }
  })
})
