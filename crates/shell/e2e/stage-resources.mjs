#!/usr/bin/env node
// Lays out the app's resources the way the installer does, for the real-app tests (gate=e2e),
// from the bundle configs of crates/shell/bundle, keeping every resource whose source was built
// on this runner and skipping (and listing) the rest: the installer-only pieces (the CLI's Node
// runtime, the update feed, the notices) are not built by this gate.
//
//   node e2e/stage-resources.mjs mac-config            prints the `--config` JSON for `tauri build`
//   node e2e/stage-resources.mjs win-stage <release>   copies them below <release> (the folder of
//                                                      SeuOffice.exe), where Tauri looks on Windows
//
// Run from anywhere; paths are those of the configs, relative to crates/shell.

import { cpSync, existsSync, mkdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const shell = resolve(dirname(fileURLToPath(import.meta.url)), '..')

/** Where a resource of the Windows config is built when this runner builds it for the host target. */
const HOST_TARGET_ALTERNATES = [['/target/x86_64-pc-windows-gnu/release/', '/target/release/']]

function resourcesOf(config) {
  return JSON.parse(readFileSync(join(shell, 'bundle', config), 'utf8')).bundle.resources
}

/** The source as it exists on this runner, or undefined. */
export function builtSource(source, exists = existsSync) {
  const direct = resolve(shell, source)
  if (exists(direct)) return direct
  for (const [from, to] of HOST_TARGET_ALTERNATES) {
    if (source.includes(from)) {
      const alternate = resolve(shell, source.replace(from, to))
      if (exists(alternate)) return alternate
    }
  }
  return undefined
}

/** Splits a resource table into what exists here (`kept`: key -> resolved source) and what does not. */
export function splitResources(resources, exists = existsSync) {
  const kept = {}
  const skipped = []
  for (const [source, destination] of Object.entries(resources)) {
    const found = builtSource(source, exists)
    if (found) kept[source] = { found, destination }
    else skipped.push(source)
  }
  return { kept, skipped }
}

function macConfig() {
  const { kept, skipped } = splitResources(resourcesOf('macos.conf.json'))
  for (const source of skipped) console.error(`not staged (not built here): ${source}`)
  const resources = Object.fromEntries(
    Object.entries(kept).map(([source, { destination }]) => [source, destination]),
  )
  console.log(JSON.stringify({ bundle: { resources } }))
}

function winStage(release) {
  const { kept, skipped } = splitResources(resourcesOf('windows.conf.json'))
  for (const [source, { found, destination }] of Object.entries(kept)) {
    const target = join(resolve(release), destination)
    mkdirSync(dirname(target), { recursive: true })
    cpSync(found, target, { recursive: statSync(found).isDirectory() })
    console.error(`staged ${source} -> ${destination}`)
  }
  for (const source of skipped) console.error(`not staged (not built here): ${source}`)
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [command, release] = process.argv.slice(2)
  if (command === 'mac-config') macConfig()
  else if (command === 'win-stage' && release) winStage(release)
  else {
    console.error('usage: stage-resources.mjs mac-config | win-stage <release dir>')
    process.exit(64)
  }
}
