import { Buffer } from 'node:buffer'
import { gunzipSync } from 'node:zlib'
import { initEngineSync } from '@seuoffice/engine-wasm'
import { initSync as initRhwp } from '../../../vendor/rhwp-core/rhwp.js'
import engineGzip from './office-engine.js'
import rhwpGzip from './rhwp-engine.js'
import { render } from './replay'

export function renderGuestBytes(bytes, history, author) {
  if (history?.format?.name === 'docx' || history?.format?.name === 'pptx')
    initEngineSync(gunzipSync(Buffer.from(engineGzip, 'base64')))
  if (history?.format?.name === 'hwp' || history?.format?.name === 'hwpx')
    initRhwp({ module: gunzipSync(Buffer.from(rhwpGzip, 'base64')) })
  return render(bytes, history, author)
}
