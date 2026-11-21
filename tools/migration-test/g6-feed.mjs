import { createReadStream, readFileSync, statSync } from 'node:fs'
import { createServer } from 'node:http'
import { basename } from 'node:path'

// Serve only files the artifact verifier selected, never the artifact directory or a redirect.
export function suffixFeed(plan, selectedStage) {
  const stages = new Map()
  for (const stage of ['first', 'next']) {
    const { files, installer, version } = plan[stage]
    const byName = new Map(files.map((file) => [basename(file.path), file]))
    const manifest = files.find((file) => file.kind === 'feed')
    if (!manifest || !byName.has(basename(installer))) throw Error(`Incomplete ${stage} feed`)
    const text = readFileSync(manifest.path, 'utf8')
    const lines = text.split(/\r?\n/)
    const urls = lines
      .filter((line) => /^\s*- url: /.test(line))
      .map((line) => line.trim().slice(7))
    if (
      !lines.includes(`version: ${version}`) ||
      !urls.includes(basename(installer)) ||
      !urls.every((name) => byName.has(name) && /^[\w.-]+$/.test(name)) ||
      !lines.every((line) =>
        /^(?:|version: [\d.]+|files:| {2}- url: [\w.-]+| {4}(?:size|blockMapSize): \d+|(?: {4})?sha512: [A-Za-z0-9+/=]+|path: [\w.-]+|releaseDate: '[\dT:.Z-]+')$/.test(
          line,
        ),
      ) ||
      !lines.includes(`path: ${basename(installer)}`)
    )
      throw Error(`Unexpected or nonlocal ${stage} manifest`)
    stages.set(stage, { byName, manifest, installer, manifestRead: false, payloadRead: false })
  }
  const server = createServer((req, res) => {
    const stage = stages.get(selectedStage())
    const path = req.url.split('?')[0]
    const file = /^\/[\w.-]+$/.test(path) && stage?.byName.get(path.slice(1))
    if (!file || !['GET', 'HEAD'].includes(req.method)) return res.writeHead(404).end()
    const size = statSync(file.path).size
    const match = /^bytes=(\d+)-(\d+)$/.exec(req.headers.range ?? '')
    const start = match ? Number(match[1]) : 0
    const end = match ? Number(match[2]) : size - 1
    if (start > end || end >= size) return res.writeHead(416).end()
    res.writeHead(match ? 206 : 200, {
      'content-length': end - start + 1,
      'accept-ranges': 'bytes',
      ...(match ? { 'content-range': `bytes ${start}-${end}/${size}` } : {}),
    })
    if (req.method === 'HEAD') return res.end()
    res.on('finish', () => {
      if (file.path === stage.manifest.path) stage.manifestRead = true
      if (file.path === stage.installer) stage.payloadRead = true
    })
    const stream = createReadStream(file.path, { start, end })
    stream.on('error', () => res.destroy())
    res.on('close', () => stream.destroy())
    stream.pipe(res)
  })
  return {
    server,
    observed(stage) {
      const s = stages.get(stage)
      return s.manifestRead && s.payloadRead
    },
  }
}
