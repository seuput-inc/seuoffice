// Static boundary checks only; this is not a native codec or pdf-lib proof.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
const build = readFileSync(join(here, 'build.rs'), 'utf8')
const runner = readFileSync(join(here, 'main.rs'), 'utf8')
const compare = readFileSync(join(here, 'compare.ts'), 'utf8')
for (const name of ['pdf_codec.rs', 'crypto/pdf_lib.rs', 'native_engine/signatures/pdf.rs']) assert(build.includes(name), `actual source missing: ${name}`)
for (const op of ['inspect', 'assemble', 'validate']) assert(runner.includes(`"${op}" =>`), `missing operation ${op}`)
for (const forbidden of ['"sign_digest" =>', '"private_key" =>', '"sign" =>']) assert(!runner.includes(forbidden), `forbidden signing route ${forbidden}`)
assert(compare.includes("../../apps/pdf/src/main/pdf-signatures"))
assert(compare.includes("../../apps/pdf/src/main/pdf-sign'"))
console.log('Static proof-boundary checks only; native execution remains required.')
