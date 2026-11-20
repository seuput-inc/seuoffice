'use strict'
// Inventory the actual installed packages used by the differential, including
// transitive and optional native dependencies. This is evidence, not a lockfile.
const fs = require('node:fs')
const path = require('node:path')
const crypto = require('node:crypto')
const { createRequire } = require('node:module')
const root = path.resolve(__dirname, '../..')
const hash = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex')
const packages = new Map()
const missingOptional = []
function packageDirectory(name, from) {
  const requireFrom = createRequire(path.join(from, 'package.json'))
  let resolved
  try {
    resolved = requireFrom.resolve(`${name}/package.json`)
  } catch (_) {
    resolved = requireFrom.resolve(name)
  }
  let directory = path.dirname(fs.realpathSync(resolved))
  for (;;) {
    const file = path.join(directory, 'package.json')
    if (fs.existsSync(file) && JSON.parse(fs.readFileSync(file, 'utf8')).name === name)
      return directory
    const parent = path.dirname(directory)
    if (parent === directory) throw new Error(`cannot locate package ${name} from ${from}`)
    directory = parent
  }
}
function visit(name, from, optional = false) {
  let directory
  try {
    directory = packageDirectory(name, from)
  } catch (error) {
    if (!optional || error.code !== 'MODULE_NOT_FOUND') throw error
    missingOptional.push(`${path.relative(root, from)}:${name}`)
    return
  }
  if (packages.has(directory)) return
  const metadata = JSON.parse(fs.readFileSync(path.join(directory, 'package.json'), 'utf8'))
  const entry = {
    name: metadata.name,
    version: metadata.version,
    path: path.relative(root, directory),
    files: {},
  }
  packages.set(directory, entry)
  function files(dir) {
    for (const item of fs
      .readdirSync(dir, { withFileTypes: true })
      .sort((a, b) => a.name.localeCompare(b.name))) {
      if (item.name === 'node_modules') continue
      const file = path.join(dir, item.name)
      if (item.isDirectory()) files(file)
      else if (item.isFile())
        entry.files[path.relative(directory, file)] = hash(fs.readFileSync(file))
      else if (item.isSymbolicLink()) {
        const target = fs.realpathSync(file)
        if (!target.startsWith(directory + path.sep) || !fs.statSync(target).isFile())
          throw new Error(`unsafe package symlink: ${file}`)
        entry.files[path.relative(directory, file)] = hash(fs.readFileSync(target))
      }
    }
  }
  files(directory)
  const optionalNames = new Set(Object.keys(metadata.optionalDependencies || {}))
  for (const dependency of Object.keys({
    ...metadata.dependencies,
    ...metadata.optionalDependencies,
  }).sort()) {
    visit(dependency, directory, optionalNames.has(dependency))
  }
}
for (const name of ['pdf-lib', 'node-forge', 'pngjs', 'tsx']) visit(name, root)
console.log(
  JSON.stringify(
    {
      packages: [...packages.values()].sort((a, b) => a.path.localeCompare(b.path)),
      missingOptional: missingOptional.sort(),
    },
    null,
    2,
  ),
)
