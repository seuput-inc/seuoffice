import { execFileSync } from 'node:child_process'
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

const [resources, directory, expectedVersion] = process.argv.slice(2)
if (!resources || !directory || !expectedVersion)
  throw new Error('usage: smoke-cli.mjs <resources> <directory> <expected-version>')
mkdirSync(directory, { recursive: true })
const node = join(resources, 'cli', 'node', process.platform === 'win32' ? 'node.exe' : 'node')
const cli = join(resources, 'cli', 'seuoffice.cjs')
const app =
  process.platform === 'win32'
    ? join(resources, '..', 'SeuOffice.exe')
    : join(resources, '..', 'MacOS', 'SeuOffice')
const { SEUOFFICE_WASM: _wasm, ...environment } = process.env
const env = { ...environment, ELECTRON_RUN_AS_NODE: '1' }
const run = (...args) =>
  execFileSync(app, [cli, ...args], {
    encoding: 'utf8',
    cwd: directory,
    env,
    stdio: ['ignore', 'pipe', 'inherit'],
  })
execFileSync(node, ['--version'], { stdio: 'inherit' })
const version = run('--version')
if (version !== `seuoffice ${expectedVersion}\n`)
  throw new Error(`unexpected CLI version: ${JSON.stringify(version)}`)
process.stdout.write(version)
writeFileSync(join(directory, 'input.md'), '# Bundle smoke\n\nSmoke document text.\n')
writeFileSync(join(directory, 'input.json'), JSON.stringify([['Smoke cell', 42]]))
run(
  'create',
  '--type',
  'docx',
  '--from',
  join(directory, 'input.md'),
  '--out',
  join(directory, 'smoke.docx'),
)
const doc = run('docs', 'read', join(directory, 'smoke.docx'), '--full', '--json')
if (JSON.parse(doc).status !== 'ok' || !doc.includes('Smoke document text'))
  throw new Error('DOCX readback did not contain the input')
run(
  'create',
  '--type',
  'xlsx',
  '--from',
  join(directory, 'input.json'),
  '--out',
  join(directory, 'smoke.xlsx'),
)
const sheet = run('sheet', 'read', join(directory, 'smoke.xlsx'), '--json')
if (JSON.parse(sheet).status !== 'ok' || !sheet.includes('Smoke cell') || !sheet.includes('42'))
  throw new Error('XLSX readback did not contain the input')
for (const file of ['smoke.docx', 'smoke.xlsx']) {
  if (readFileSync(join(directory, file)).length === 0) throw new Error(`${file} is empty`)
}
