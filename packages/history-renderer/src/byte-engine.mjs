import { Buffer } from 'node:buffer'
import { gunzipSync } from 'node:zlib'
import { initEngineSync } from '@seuoffice/engine-wasm'
import engineGzip from './office-engine.js'
import { render } from './replay'

export function renderGuestBytes(bytes, history, author) {
  if (history?.format?.name === 'docx' || history?.format?.name === 'pptx')
    initEngineSync(gunzipSync(Buffer.from(engineGzip, 'base64')))
  return render(bytes, history, author)
}
