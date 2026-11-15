import { createHash } from 'node:crypto'
import { createReadStream } from 'node:fs'
import { readFile, readdir, stat, writeFile } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { join } from 'node:path'
import { parseArgs } from 'node:util'

const { values } = parseArgs({ options: {
  root: { type: 'string' }, platform: { type: 'string' }, versions: { type: 'string' },
  source: { type: 'string' }, definition: { type: 'string' }, run: { type: 'string' },
  signing: { type: 'string' },
} })
const { root, platform, source, definition, run, signing } = values
const versions = (values.versions || '').split(' ').filter(Boolean)
if (!root || !['win32', 'darwin'].includes(platform) || !/^[a-f0-9]{40}$/.test(source || '') ||
    !/^[a-f0-9]{40}$/.test(definition || '') || !/^[0-9]+$/.test(run || '') ||
    !(platform === 'win32' ? ['unsigned', 'authenticode'] : ['ad-hoc', 'developer-id-notarized']).includes(signing) ||
    !versions.length || new Set(versions).size !== versions.length ||
    versions.some((version) => !/^\d+\.\d+\.\d+$/.test(version))) {
  throw new Error('invalid manifest arguments')
}
const require = createRequire(import.meta.url)
const yaml = require('js-yaml')
const digest = async (file, algorithm, encoding) => {
  const hash = createHash(algorithm)
  for await (const chunk of createReadStream(file)) hash.update(chunk)
  return hash.digest(encoding)
}
const result = []
for (const version of versions) {
  const dir = join(root, version)
  const channels = []
  const expected = new Set()
  for (const channel of ['latest', 'stable']) {
    const suffix = channel === 'stable' ? '-stable' : ''
    const entries = platform === 'win32'
      ? [[`SeuOffice-Setup-${version}${suffix}.exe`, 'installer']]
      : [[`SeuOffice-${version}-arm64${suffix}.dmg`, 'installer'],
          [`SeuOffice-${version}-arm64-mac${suffix}.zip`, 'archive']]
    const update = entries.at(-1)[0]
    entries.push([`${update}.blockmap`, 'blockmap'])
    const feed = `${channel}${platform === 'darwin' ? '-mac' : ''}.yml`
    entries.push([feed, 'feed'])
    const parsed = yaml.load(await readFile(join(dir, feed), 'utf8'))
    if (!parsed || parsed.version !== version || !Array.isArray(parsed.files) ||
        parsed.files.length !== (platform === 'darwin' ? 2 : 1)) throw new Error(`invalid ${feed}`)
    const feedFiles = new Map(parsed.files.map((entry) => [entry.url, entry]))
    if (feedFiles.size !== parsed.files.length || parsed.path !== parsed.files[0].url ||
        parsed.sha512 !== parsed.files[0].sha512) throw new Error(`invalid ${feed} entries`)
    for (const [name] of entries.slice(0, platform === 'darwin' ? 2 : 1)) {
      const entry = feedFiles.get(name)
      const file = join(dir, name)
      const size = (await stat(file)).size
      if (!entry || entry.size !== size || entry.sha512 !== await digest(file, 'sha512', 'base64')) {
        throw new Error(`feed mismatch for ${name}`)
      }
    }
    const files = []
    for (const [name, kind] of entries) {
      if (expected.has(name)) throw new Error(`duplicate file ${name}`)
      expected.add(name)
      const file = join(dir, name)
      const info = await stat(file)
      if (!info.isFile() || info.size === 0) throw new Error(`invalid file ${file}`)
      files.push({ name, relativePath: `${version}/${name}`, sha256: await digest(file, 'sha256', 'hex'), bytes: info.size, kind })
    }
    channels.push({ channel, signing, files })
  }
  const actual = await readdir(dir)
  if (actual.length !== expected.size || actual.some((name) => !expected.has(name))) {
    throw new Error(`unexpected files in ${dir}`)
  }
  result.push({ version, channels })
}
await writeFile(join(root, 'rust-bundle-manifest.json'), `${JSON.stringify({
  schema: 'seuoffice-rust-bundle/v1', sourceSha: source, definitionSha: definition,
  workflowRunId: run, platform, arch: platform === 'win32' ? 'x64' : 'arm64', versions: result,
}, null, 2)}\n`)
