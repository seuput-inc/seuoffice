import { describe, expect, it } from 'vitest'
import { NATIVE_CHANNEL_CONTRACTS } from '../shell-inventory/native-channels.ts'
import { SYNTHETIC_ONLY } from '../shell-inventory/native-waivers.mjs'
import type { CanonicalSpec } from './channels.ts'
import {
  NATIVE_HOST_PENDING,
  blockingProblems,
  nativeContractCoverage,
  nativeEvidence,
  type NativeEvidence,
} from './native-contracts.ts'

const contract = NATIVE_CHANNEL_CONTRACTS[0]!
const specs: CanonicalSpec[] = [
  {
    name: contract.name,
    namespace: 'native',
    batch: 'D2',
    kinds: ['invoke'],
    surfaces: ['Docs'],
  },
]
/** The scenarios of the contract the real app cannot reach (owner-loss, effect-failure) */
const waived = Object.keys(SYNTHETIC_ONLY[contract.name] ?? {})

/** Accounting fixtures only. They are not evidence of native execution. */
function receipt(
  kind: NativeEvidence['kind'],
  status: NativeEvidence['status'] = 'passed',
): NativeEvidence {
  return {
    channel: contract.name,
    kind,
    status,
    scenarios: contract.requiredScenarios.filter(
      (scenario) => kind === 'synthetic' || !waived.includes(scenario),
    ),
    artifact: `fixture-${kind}.log`,
  }
}

describe('separate native-contract evidence accounting', () => {
  it('keeps declared but unexecuted behavior and native authority pending', () => {
    const [report] = nativeContractCoverage('D2', specs)
    expect(report!.synthetic.status).toBe('pending')
    expect(report!.nativeHost.status).toBe('pending')
    expect(report!.synthetic.missingScenarios).toEqual(contract.requiredScenarios)
    expect(report!.problems).toContain('native authority/effect proof pending')
  })

  it('cannot treat complete synthetic coverage as native authority proof', () => {
    const [report] = nativeContractCoverage('D2', specs, [receipt('synthetic')])
    expect(report!.synthetic.status).toBe('passed')
    expect(report!.nativeHost.status).toBe('pending')
    expect(report!.problems).not.toEqual([])
  })

  it('requires every scenario independently on both evidence tracks', () => {
    const synthetic = receipt('synthetic')
    const native = { ...receipt('native-host'), scenarios: ['normal-completion'] }
    const [partial] = nativeContractCoverage('D2', specs, [synthetic, native])
    expect(partial!.nativeHost.status).toBe('pending')
    expect(partial!.nativeHost.missingScenarios).toContain('missing-token')
    const [complete] = nativeContractCoverage('D2', specs, [synthetic, receipt('native-host')])
    expect(complete!.problems).toEqual([])
  })

  it('propagates failed native evidence even when synthetic evidence passed', () => {
    const [report] = nativeContractCoverage('D2', specs, [
      receipt('synthetic'),
      receipt('native-host', 'failed'),
    ])
    expect(report!.nativeHost.status).toBe('failed')
    expect(report!.problems).toContain('native authority/effect proof failed')
  })

  it('does not let receipts excuse missing native registration', () => {
    const [report] = nativeContractCoverage(
      'D2',
      [],
      [receipt('synthetic'), receipt('native-host')],
    )
    expect(report!.problems).toContain('missing native namespace registration')
  })

  it('keeps the actual required surface and owner batch in the contract', () => {
    const wrong = [{ ...specs[0]!, surfaces: ['Shell'], batch: 'M' }]
    const [report] = nativeContractCoverage('D2', wrong, [
      receipt('synthetic'),
      receipt('native-host'),
    ])
    expect(report!.problems).toContain('native registration batch, kinds or surfaces differ')
  })

  it('rejects undeclared native channels instead of granting a blanket exception', () => {
    expect(() =>
      nativeContractCoverage('D2', [{ ...specs[0]!, name: 'docs:another-native-route' }]),
    ).toThrow('lack explicit contracts')
  })

  it('rejects parity results, missing artifacts and unknown scenarios as receipts', () => {
    expect(() => nativeEvidence([{ ...receipt('synthetic'), kind: 'electron-parity' }])).toThrow()
    expect(() => nativeEvidence([{ ...receipt('native-host'), artifact: '' }])).toThrow()
    expect(() =>
      nativeEvidence([{ ...receipt('native-host'), scenarios: ['source-text-wiring'] }]),
    ).toThrow('unknown native evidence scenario')
    expect(nativeEvidence([receipt('synthetic')])).toEqual([receipt('synthetic')])
  })
})

describe('the native-host waiver of the scenarios the real app cannot reach', () => {
  it('is exactly the eleven scenarios of the decision, each with its reason', () => {
    const pairs = Object.entries(SYNTHETIC_ONLY).flatMap(([channel, scenarios]) =>
      Object.entries(scenarios).map(([scenario, reason]) => {
        expect(reason.trim(), `${channel} ${scenario}`).not.toBe('')
        return `${channel} ${scenario}`
      }),
    )
    // hub-g4-contract-decision-20261120: a new entry needs a new decision
    expect(pairs.sort()).toEqual([
      'docs:owned-print-pdf-buffer effect-failure',
      'docs:owned-print-pdf-buffer owner-loss',
      'html:recovery-applied failed-apply',
      'html:saved-input-applied failed-apply',
      'markdown:recovery-applied failed-apply',
      'markdown:saved-input-applied failed-apply',
      'pdf:consume-recovery-pending preparation-loss',
      'pdf:recovery-initialized revision-loss',
      'slides:recovery-initialized failed-apply',
      'workbook:saved-open-result closed-tab',
      'workbook:saved-open-result missing-native-authority',
    ])
  })

  it('lets native-host proof pass without the waived scenarios and synthetic proof not', () => {
    const [report] = nativeContractCoverage('D2', specs, [
      receipt('synthetic'),
      receipt('native-host'),
    ])
    expect(report!.nativeHost).toMatchObject({ status: 'passed', missingScenarios: [] })
    expect(report!.nativeHost.waivedScenarios).toEqual(['owner-loss', 'effect-failure'])
    expect(report!.synthetic.waivedScenarios).toEqual([])
    expect(report!.problems).toEqual([])

    // a waived scenario still needs its synthetic receipt
    const synthetic = {
      ...receipt('synthetic'),
      scenarios: contract.requiredScenarios.filter((scenario) => scenario !== 'owner-loss'),
    }
    const [missing] = nativeContractCoverage('D2', specs, [synthetic, receipt('native-host')])
    expect(missing!.synthetic).toMatchObject({
      status: 'pending',
      missingScenarios: ['owner-loss'],
    })
    expect(missing!.problems).toEqual(['synthetic behavior pending'])
    // and a failed one fails
    const [failed] = nativeContractCoverage('D2', specs, [
      receipt('synthetic'),
      { ...receipt('synthetic', 'failed'), scenarios: ['owner-loss'] },
      receipt('native-host'),
    ])
    expect(failed!.problems).toEqual(['synthetic behavior failed'])
  })

  it('fails when a native-host receipt names a waived scenario: a spec drives it', () => {
    for (const status of ['passed', 'failed'] as const) {
      const [report] = nativeContractCoverage('D2', specs, [
        receipt('synthetic'),
        receipt('native-host'),
        { ...receipt('native-host', status), scenarios: ['owner-loss'] },
      ])
      expect(report!.problems).toContain(
        'native-host evidence of the waived scenario owner-loss: drop its waiver',
      )
      expect(blockingProblems(report!, 'warn')).not.toEqual([])
    }
  })

  it('refuses a waiver of a scenario no contract requires', () => {
    const waive = (waivers: Record<string, Record<string, string>>) => () =>
      nativeContractCoverage('D2', specs, [], waivers)
    expect(waive({ [contract.name]: { 'no-such-scenario': 'reason' } })).toThrow(
      'names no required',
    )
    expect(waive({ 'docs:no-such-channel': { 'owner-loss': 'reason' } })).toThrow(
      'names no required',
    )
    expect(waive({ [contract.name]: { 'owner-loss': ' ' } })).toThrow('no reason')
  })
})

describe('a run without native-host evidence', () => {
  it('warns of a pending native-host proof and still fails on everything else', () => {
    const [pending] = nativeContractCoverage('D2', specs, [receipt('synthetic')])
    expect(pending!.problems).toEqual([NATIVE_HOST_PENDING])
    expect(blockingProblems(pending!, 'fail')).toEqual([NATIVE_HOST_PENDING])
    expect(blockingProblems(pending!, 'warn')).toEqual([])

    const [synthetic] = nativeContractCoverage('D2', specs)
    expect(blockingProblems(synthetic!, 'warn')).toEqual(['synthetic behavior pending'])
    const [failed] = nativeContractCoverage('D2', specs, [
      receipt('synthetic'),
      receipt('native-host', 'failed'),
    ])
    expect(blockingProblems(failed!, 'warn')).toEqual(['native authority/effect proof failed'])
    const [unregistered] = nativeContractCoverage('D2', [], [receipt('synthetic')])
    expect(blockingProblems(unregistered!, 'warn')).toEqual([
      'missing native namespace registration',
    ])
  })
})
