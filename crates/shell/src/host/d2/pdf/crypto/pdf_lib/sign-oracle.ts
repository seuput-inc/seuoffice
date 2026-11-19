// Hosted differential oracle. Inputs must be synthetic PDFs and synthetic PKCS#12 fixtures.
import { signPdf, type SignPdfOptions } from '../../../../../../../../apps/pdf/src/main/pdf-sign'

export interface SignFixture {
  pdf: Uint8Array
  p12: Uint8Array
  password: string
  signingTime: string
  reason?: string
  location?: string
  contactInfo?: string
  certifyLevel?: 1 | 2 | 3
  visible?: SignPdfOptions['visible']
}

export async function signOracle(fixture: SignFixture): Promise<Uint8Array> {
  return signPdf(fixture.pdf, {
    p12: fixture.p12,
    password: fixture.password,
    signingTime: new Date(fixture.signingTime),
    reason: fixture.reason,
    location: fixture.location,
    contactInfo: fixture.contactInfo,
    certifyLevel: fixture.certifyLevel,
    visible: fixture.visible,
  })
}
