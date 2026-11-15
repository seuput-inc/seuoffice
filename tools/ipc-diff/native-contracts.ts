import { NATIVE_CHANNEL_CONTRACTS, type NativeChannelContract } from '../shell-inventory/native-channels.ts'
import type { CanonicalSpec } from './channels.ts'

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
    if (typeof entry !== 'object' || entry === null) throw new Error('invalid native evidence entry')
    const { channel, kind, status, scenarios, artifact } = entry as Record<string, unknown>
    if (typeof channel !== 'string' || !NATIVE_CHANNEL_CONTRACTS.some((contract) => contract.name === channel)
      || (kind !== 'synthetic' && kind !== 'native-host')
      || (status !== 'passed' && status !== 'failed')
      || !Array.isArray(scenarios) || !scenarios.every((scenario): scenario is string => typeof scenario === 'string')
      || typeof artifact !== 'string' || artifact.trim().length === 0) {
      throw new Error('native evidence requires a declared channel, proof kind, status, scenarios and artifact')
    }
    const contract = NATIVE_CHANNEL_CONTRACTS.find((contract) => contract.name === channel)!
    if (scenarios.some((scenario) => !contract.requiredScenarios.includes(scenario))) {
      throw new Error(`${channel}: unknown native evidence scenario`)
    }
    return { channel, kind, status, scenarios, artifact }
  })
}

function proof(kind: NativeEvidence['kind'], contract: NativeChannelContract, receipts: readonly NativeEvidence[]): ProofTrack {
  const matching = receipts.filter((receipt) => receipt.channel === contract.name && receipt.kind === kind)
  const passed = new Set(matching.filter((receipt) => receipt.status === 'passed').flatMap((receipt) => receipt.scenarios))
  const missingScenarios = contract.requiredScenarios.filter((scenario) => !passed.has(scenario))
  return {
    status: matching.some((receipt) => receipt.status === 'failed') ? 'failed' : missingScenarios.length > 0 ? 'pending' : 'passed',
    missingScenarios,
    artifacts: matching.map((receipt) => receipt.artifact),
  }
}

const pascal = (value: string): string => value.replace(/(^|-)([a-z0-9])/g, (_, __, letter: string) => letter.toUpperCase())

/** Both namespaces remain in the runtime table. Only Electron has a differential oracle. */
export function nativeContractCoverage(batch: string, specs: readonly CanonicalSpec[], receipts: readonly NativeEvidence[] = []): NativeContractReport[] {
  const contracts = NATIVE_CHANNEL_CONTRACTS.filter((contract) => contract.batch === batch)
  const unknown = specs.filter((spec) => spec.batch === batch && spec.namespace === 'native'
    && !contracts.some((contract) => contract.name === spec.name))
  if (unknown.length > 0) throw new Error(`native channels lack explicit contracts: ${unknown.map((spec) => spec.name).join(', ')}`)
  return contracts.map((contract) => {
    const spec = specs.find((spec) => spec.name === contract.name)
    const problems: string[] = []
    if (!spec || spec.namespace !== 'native') problems.push('missing native namespace registration')
    const expectedSurfaces = contract.surfaces.map(pascal)
    if (spec && (spec.batch !== contract.batch
      || spec.kinds.length !== contract.kinds.length || spec.kinds.some((kind) => !contract.kinds.some((expected) => expected === kind))
      || spec.surfaces.length !== expectedSurfaces.length || spec.surfaces.some((surface) => !expectedSurfaces.includes(surface)))) {
      problems.push('native registration batch, kinds or surfaces differ')
    }
    const synthetic = proof('synthetic', contract, receipts)
    const nativeHost = proof('native-host', contract, receipts)
    if (synthetic.status !== 'passed') problems.push(`synthetic behavior ${synthetic.status}`)
    if (nativeHost.status !== 'passed') problems.push(`native authority/effect proof ${nativeHost.status}`)
    return { channel: contract.name, batch, contract, problems, synthetic, nativeHost }
  })
}
