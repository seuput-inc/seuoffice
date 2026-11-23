import { execFileSync } from 'node:child_process'
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

// The release job stamps packages before packing. Stage a pair without touching its worktree.
const [version, output] = process.argv.slice(2)
if (!/^\d+\.\d+\.\d+(?:-rc\.\d+)?$/.test(version ?? '') || !output) {
  throw new Error('usage: node pack-pair.mjs <version> <output-directory>')
}
const out = resolve(output)
mkdirSync(out, { recursive: true })
const temp = mkdtempSync(join(out, '.history-pack-'))
try {
  for (const name of ['collab', 'history-renderer']) {
    const src = fileURLToPath(new URL(`../${name}/`, import.meta.url))
    execFileSync('npm', ['run', 'prepack'], { cwd: src, stdio: 'inherit' })
    const manifest = JSON.parse(readFileSync(join(src, 'package.json'), 'utf8'))
    const cwd = join(temp, name)
    mkdirSync(cwd)
    for (const file of manifest.files) cpSync(join(src, file), join(cwd, file), { recursive: true })
    manifest.version = version
    if (name === 'history-renderer') manifest.dependencies['@seuoffice/collab'] = version
    writeFileSync(join(cwd, 'package.json'), JSON.stringify(manifest, null, 2) + '\n')
    execFileSync('npm', ['pack', '--ignore-scripts', '--offline', '--pack-destination', out], {
      cwd,
      stdio: 'inherit',
    })
  }
} finally {
  rmSync(temp, { recursive: true, force: true })
}
