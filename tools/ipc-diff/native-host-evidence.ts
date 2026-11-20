/**
 * Takes the native-host receipts of one `seuoffice shell` `gate=e2e` run into the evidence
 * file of the IPC comparison (`--native-evidence`), beside the synthetic receipts.
 *
 *   npm --prefix tools/ipc-diff run native-host -- --sha <commit> --mac <dir> --windows <dir>
 *                                                  --evidence <merged.json>
 *
 * `<dir>` is the downloaded `e2e-macOS` / `e2e-Windows` artifact, which holds
 * `native-host/receipts.json` (crates/shell/ci/native-host-check.mjs) and the records it names.
 * The receipts are executed evidence of one commit on one OS:
 *
 * - a report of another commit or another OS, or one whose records are missing, is refused:
 *   nothing is merged and the exit status is 1, so the native-host track stays pending;
 * - so is a report with a receipt that names a scenario the comparison waives from the
 *   native-host proof (tools/shell-inventory/native-waivers.mjs): a spec that reaches it makes
 *   the waiver wrong. This is checked on each OS's receipts, before the two are intersected,
 *   which would drop such a scenario passed on one OS only;
 * - a scenario passes only where the macOS and the Windows receipts both pass it;
 * - a failed receipt of either OS stays a failed receipt.
 */
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { SYNTHETIC_ONLY } from '../shell-inventory/native-waivers.mjs'
import type { NativeEvidence } from './native-contracts.ts'

export const HOSTS = ['macOS', 'Windows'] as const
export type Host = (typeof HOSTS)[number]

export interface HostReceipt extends NativeEvidence {
  os: Host
  spec: string
}

/** `native-host/receipts.json` below `dir`: where the artifact's root is depends on what the job uploaded */
export function findReport(dir: string, depth = 3): string | undefined {
  const direct = join(dir, 'native-host', 'receipts.json')
  if (existsSync(direct)) return direct
  if (depth === 0 || !existsSync(dir)) return undefined
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue
    const found = findReport(join(dir, entry.name), depth - 1)
    if (found) return found
  }
  return undefined
}

/** The receipts of one OS, their records resolved; throws when they are not this commit's and this OS's */
export function loadReport(dir: string, os: Host, sha: string): HostReceipt[] {
  const file = findReport(dir)
  if (!file) throw new Error(`${os}: no native-host/receipts.json below ${dir}`)
  const report = JSON.parse(readFileSync(file, 'utf8')) as {
    os?: unknown
    head_sha?: unknown
    receipts?: unknown
  }
  if (report.head_sha !== sha)
    throw new Error(`${os}: the receipts are of commit ${String(report.head_sha)}, not ${sha}`)
  if (report.os !== os) throw new Error(`${os}: the receipts are of ${String(report.os)}`)
  if (!Array.isArray(report.receipts)) throw new Error(`${os}: the report has no receipts`)
  return (report.receipts as HostReceipt[]).map((receipt) => {
    if (receipt.kind !== 'native-host' || receipt.os !== os)
      throw new Error(
        `${os}: a receipt of ${receipt.channel} is not a native-host receipt of ${os}`,
      )
    const waived = receipt.scenarios.filter(
      (scenario) => SYNTHETIC_ONLY[receipt.channel]?.[scenario] !== undefined,
    )
    if (waived.length > 0)
      throw new Error(
        `${os}: a receipt of ${receipt.channel} names the waived scenario ${waived.join(', ')}: ` +
          'a spec drives it, so drop its waiver (tools/shell-inventory/native-waivers.mjs)',
      )
    const artifact = resolve(dirname(file), receipt.artifact)
    if (!existsSync(artifact))
      throw new Error(`${os}: the record ${receipt.artifact} of ${receipt.channel} is missing`)
    return { ...receipt, artifact }
  })
}

/** Passed scenarios of both OSes, and every failed receipt */
export function mergeHosts(reports: Record<Host, readonly HostReceipt[]>): HostReceipt[] {
  const passedOn = (os: Host, channel: string): Set<string> =>
    new Set(
      reports[os]
        .filter((receipt) => receipt.channel === channel && receipt.status === 'passed')
        .flatMap((receipt) => receipt.scenarios),
    )
  return HOSTS.flatMap((os) =>
    reports[os].flatMap((receipt) => {
      if (receipt.status === 'failed') return [receipt]
      const scenarios = receipt.scenarios.filter((scenario) =>
        HOSTS.every((host) => passedOn(host, receipt.channel).has(scenario)),
      )
      return scenarios.length > 0 ? [{ ...receipt, scenarios }] : []
    }),
  )
}

/** One line per channel: what both OSes passed, what one did, and where a receipt failed */
export function describe(reports: Record<Host, readonly HostReceipt[]>): string[] {
  const channels = [...new Set(HOSTS.flatMap((os) => reports[os].map((r) => r.channel)))].sort()
  return channels.map((channel) => {
    const of = (os: Host, status: NativeEvidence['status']): string[] => [
      ...new Set(
        reports[os]
          .filter((receipt) => receipt.channel === channel && receipt.status === status)
          .flatMap((receipt) => receipt.scenarios),
      ),
    ]
    const [mac, windows] = [of('macOS', 'passed'), of('Windows', 'passed')]
    const both = mac.filter((scenario) => windows.includes(scenario))
    const parts = [`both ${both.sort().join(', ') || 'none'}`]
    for (const [os, passed] of [
      ['macOS', mac],
      ['Windows', windows],
    ] as const) {
      const alone = passed.filter((scenario) => !both.includes(scenario))
      if (alone.length > 0) parts.push(`${os} only ${alone.sort().join(', ')}`)
      const failed = of(os, 'failed')
      if (failed.length > 0) parts.push(`failed on ${os} ${failed.sort().join(', ')}`)
    }
    return `${channel}: ${parts.join('; ')}`
  })
}

/** The evidence file with this run's native-host receipts in place of any it held */
export function mergeInto(evidenceFile: string, receipts: readonly HostReceipt[]): number {
  const kept = existsSync(evidenceFile)
    ? (JSON.parse(readFileSync(evidenceFile, 'utf8')) as NativeEvidence[]).filter(
        (receipt) => receipt.kind !== 'native-host',
      )
    : []
  writeFileSync(evidenceFile, `${JSON.stringify([...kept, ...receipts], null, 2)}\n`)
  return kept.length
}

/** The command: 0 when the receipts of both OSes were merged into the evidence file, else 1 or 2 */
export function main(argv: readonly string[]): number {
  const option = (name: string): string | undefined => {
    const at = argv.indexOf(name)
    return at >= 0 ? argv[at + 1] : undefined
  }
  const [sha, mac, windows, evidence] = ['--sha', '--mac', '--windows', '--evidence'].map(option)
  if (!sha || !mac || !windows || !evidence) {
    console.error(
      'usage: native-host-evidence.ts --sha <commit> --mac <dir> --windows <dir> --evidence <merged.json>',
    )
    return 2
  }
  let reports: Record<Host, HostReceipt[]>
  try {
    reports = { macOS: loadReport(mac, 'macOS', sha), Windows: loadReport(windows, 'Windows', sha) }
  } catch (error) {
    console.error(`native-host receipts refused: ${(error as Error).message}`)
    return 1
  }
  for (const line of describe(reports)) console.log(line)
  const merged = mergeHosts(reports)
  const kept = mergeInto(evidence, merged)
  console.log(`${merged.length} native-host receipts beside ${kept} others -> ${evidence}`)
  return 0
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = main(process.argv.slice(2))
}
