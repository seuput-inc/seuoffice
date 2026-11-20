import { describe, expect, it } from 'vitest'
import { NATIVE_CHANNEL_CONTRACTS } from '../shell-inventory/native-channels.ts'
import type { CanonicalSpec } from './channels.ts'
import { nativeContractCoverage, nativeEvidence, type NativeEvidence } from './native-contracts.ts'

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

/** Accounting fixtures only. They are not evidence of native execution. */
function receipt(
  kind: NativeEvidence['kind'],
  status: NativeEvidence['status'] = 'passed',
): NativeEvidence {
  return {
    channel: contract.name,
    kind,
    status,
    scenarios: [...contract.requiredScenarios],
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
    expect(partial!.nativeHost.missingScenarios).toContain('owner-loss')
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
