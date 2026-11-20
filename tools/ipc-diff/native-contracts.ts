import {
  NATIVE_CHANNEL_CONTRACTS,
  type NativeChannelContract,
} from '../shell-inventory/native-channels.ts'
import { SYNTHETIC_ONLY } from '../shell-inventory/native-waivers.mjs'
import type { CanonicalSpec } from './channels.ts'

/** Channel, then scenario, then why the real app cannot reach it (native-waivers.mjs) */
export type NativeHostWaivers = Readonly<Record<string, Readonly<Record<string, string>>>>

/** The problem of a native-host proof that has no receipts yet */
export const NATIVE_HOST_PENDING = 'native authority/effect proof pending'

export interface NativeEvidence {
  channel: string
  kind: 'synthetic' | 'native-host'
  status: 'passed' | 'failed'
  scenarios: string[]
  artifact: string
}

interface ProofTrack {
  status: 'pending' | 'failed' | 'passed'
  missingScenarios: string[]
  /** required scenarios this track does not ask for: the native-host waiver */
  waivedScenarios: string[]
  artifacts: string[]
}

export interface NativeContractReport {
  channel: string
  batch: string
  contract: NativeChannelContract
  problems: string[]
  synthetic: ProofTrack
  nativeHost: ProofTrack
}

/** Receipts describe executed evidence; test source and Electron diffs are not receipts. */
export function nativeEvidence(value: unknown): NativeEvidence[] {
  if (!Array.isArray(value)) throw new Error('native evidence must be an array')
  return value.map((entry: unknown) => {
    if (typeof entry !== 'object' || entry === null)
      throw new Error('invalid native evidence entry')
    const { channel, kind, status, scenarios, artifact } = entry as Record<string, unknown>
    if (
      typeof channel !== 'string' ||
      !NATIVE_CHANNEL_CONTRACTS.some((contract) => contract.name === channel) ||
      (kind !== 'synthetic' && kind !== 'native-host') ||
      (status !== 'passed' && status !== 'failed') ||
      !Array.isArray(scenarios) ||
      !scenarios.every((scenario): scenario is string => typeof scenario === 'string') ||
      typeof artifact !== 'string' ||
      artifact.trim().length === 0
    ) {
      throw new Error(
        'native evidence requires a declared channel, proof kind, status, scenarios and artifact',
      )
    }
    const contract = NATIVE_CHANNEL_CONTRACTS.find((contract) => contract.name === channel)!
    if (scenarios.some((scenario) => !contract.requiredScenarios.includes(scenario))) {
      throw new Error(`${channel}: unknown native evidence scenario`)
    }
    return { channel, kind, status, scenarios, artifact }
  })
}

function proof(
  kind: NativeEvidence['kind'],
  contract: NativeChannelContract,
  receipts: readonly NativeEvidence[],
  waived: readonly string[] = [],
): ProofTrack {
  const matching = receipts.filter(
    (receipt) => receipt.channel === contract.name && receipt.kind === kind,
  )
  const passed = new Set(
    matching
      .filter((receipt) => receipt.status === 'passed')
      .flatMap((receipt) => receipt.scenarios),
  )
  const missingScenarios = contract.requiredScenarios.filter(
    (scenario) => !waived.includes(scenario) && !passed.has(scenario),
  )
  return {
    status: matching.some((receipt) => receipt.status === 'failed')
      ? 'failed'
      : missingScenarios.length > 0
        ? 'pending'
        : 'passed',
    missingScenarios,
    waivedScenarios: [...waived],
    artifacts: matching.map((receipt) => receipt.artifact),
  }
}

const pascal = (value: string): string =>
  value.replace(/(^|-)([a-z0-9])/g, (_, __, letter: string) => letter.toUpperCase())

/**
 * Both namespaces remain in the runtime table. Only Electron has a differential oracle.
 * Synthetic proof covers every required scenario; native-host proof every one but the waived.
 */
export function nativeContractCoverage(
  batch: string,
  specs: readonly CanonicalSpec[],
  receipts: readonly NativeEvidence[] = [],
  waivers: NativeHostWaivers = SYNTHETIC_ONLY,
): NativeContractReport[] {
  for (const [channel, scenarios] of Object.entries(waivers)) {
    const contract = NATIVE_CHANNEL_CONTRACTS.find((contract) => contract.name === channel)
    for (const [scenario, reason] of Object.entries(scenarios)) {
      if (!contract?.requiredScenarios.includes(scenario) || reason.trim().length === 0)
        throw new Error(
          `${channel}: the waiver of ${scenario} names no required scenario or no reason`,
        )
    }
  }
  const contracts = NATIVE_CHANNEL_CONTRACTS.filter((contract) => contract.batch === batch)
  const unknown = specs.filter(
    (spec) =>
      spec.batch === batch &&
      spec.namespace === 'native' &&
      !contracts.some((contract) => contract.name === spec.name),
  )
  if (unknown.length > 0)
    throw new Error(
      `native channels lack explicit contracts: ${unknown.map((spec) => spec.name).join(', ')}`,
    )
  return contracts.map((contract) => {
    const spec = specs.find((spec) => spec.name === contract.name)
    const problems: string[] = []
    if (!spec || spec.namespace !== 'native') problems.push('missing native namespace registration')
    const expectedSurfaces = contract.surfaces.map(pascal)
    if (
      spec &&
      (spec.batch !== contract.batch ||
        spec.kinds.length !== contract.kinds.length ||
        spec.kinds.some((kind) => !contract.kinds.some((expected) => expected === kind)) ||
        spec.surfaces.length !== expectedSurfaces.length ||
        spec.surfaces.some((surface) => !expectedSurfaces.includes(surface)))
    ) {
      problems.push('native registration batch, kinds or surfaces differ')
    }
    const synthetic = proof('synthetic', contract, receipts)
    const waived = Object.keys(waivers[contract.name] ?? {})
    const nativeHost = proof('native-host', contract, receipts, waived)
    if (synthetic.status !== 'passed') problems.push(`synthetic behavior ${synthetic.status}`)
    if (nativeHost.status !== 'passed')
      problems.push(`native authority/effect proof ${nativeHost.status}`)
    // a spec that reaches a waived scenario in the real app makes its waiver wrong
    const reached = receipts
      .filter((receipt) => receipt.channel === contract.name && receipt.kind === 'native-host')
      .flatMap((receipt) => receipt.scenarios)
      .filter((scenario) => waived.includes(scenario))
    for (const scenario of new Set(reached))
      problems.push(`native-host evidence of the waived scenario ${scenario}: drop its waiver`)
    return { channel: contract.name, batch, contract, problems, synthetic, nativeHost }
  })
}

/**
 * The problems of a report that fail the comparison. A run with no native-host input (a push
 * of main) says a pending native-host proof instead (`warn`); a failed one still fails.
 */
export function blockingProblems(
  report: NativeContractReport,
  nativeHostPending: 'fail' | 'warn',
): string[] {
  return nativeHostPending === 'warn'
    ? report.problems.filter((problem) => problem !== NATIVE_HOST_PENDING)
    : report.problems
}
