// A QA suffix result is never a release receipt: the original Electron hop cannot run here.
import { execFileSync, spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { createInterface } from 'node:readline'
import { pathToFileURL } from 'node:url'
import { verifyBridge } from './bridge-artifact.mjs'
import { suffixFeed } from './g6-feed.mjs'

const read = (path) => JSON.parse(readFileSync(path, 'utf8'))
const json = (path, data) => writeFileSync(path, JSON.stringify(data, null, 2) + '\n')
const version = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/

export function targets(env) {
  const [bridgeRun, bridgeVersion, bridgeSha, ...extra] = (env.G6_BRIDGE ?? '').split(':')
  const result = [
    [bridgeRun, bridgeVersion, bridgeSha],
    [env.TO_RUN, env.TO_VERSION, env.TO_SOURCE_SHA],
    [env.NEXT_RUN, env.NEXT_VERSION, env.NEXT_SOURCE_SHA],
  ]
  if (extra.length || !['stable', 'latest'].includes(env.G6_CHANNEL))
    throw Error('Invalid QA selection')
  for (const [id, v, sha] of result) {
    if (!/^[1-9]\d*$/.test(id) || !version.test(v) || !/^[a-f0-9]{40}$/.test(sha))
      throw Error('Expected pinned run:version:sha for each build')
  }
  for (let i = 1; i < 3; ++i) {
    const prev = result[i - 1][1].split('.').map(BigInt)
    const curr = result[i][1].split('.').map(BigInt)
    const different = curr.findIndex((n, k) => n !== prev[k])
    if (different < 0 || curr[different] < prev[different])
      throw Error('QA versions must strictly increase')
  }
  return result
}

export function initialReport(platform, channel) {
  return {
    schema: 'seuoffice-g6-loopback/v1',
    platform,
    channel,
    promotable: false,
    g6: 'BLOCKED',
    preparation: 'NOT_RUN',
    execution: 'NOT_RUN',
    hops: [
      {
        from: 'origin',
        to: 'bridge',
        status: 'BLOCKED',
        reason:
          'The released Electron uses its packaged GitHub provider. No supported isolated endpoint.',
      },
      { from: 'bridge', to: 'first', status: 'NOT_RUN' },
      { from: 'first', to: 'next', status: 'NOT_RUN' },
    ],
  }
}

export function recordArrival(report, plan, evidence, observed) {
  const hop = report.hops.find((h) => h.status === 'RUNNING')
  if (
    !hop ||
    evidence.stage !== hop.to ||
    evidence.version !== plan[hop.to].version ||
    !observed(hop.to) ||
    evidence.recent !== true ||
    typeof evidence.profile !== 'string' ||
    !evidence.profile ||
    !/^[a-f0-9]{64}$/.test(evidence.document)
  )
    throw Error('Missing or out-of-order installed update evidence')
  const previous = report.hops[1]
  if (
    hop.to === 'next' &&
    (previous.profile !== evidence.profile || previous.document !== evidence.document)
  )
    throw Error('Profile or document changed')
  Object.assign(hop, {
    status: 'PASSED',
    version: evidence.version,
    profile: evidence.profile,
    document: evidence.document,
    recent: true,
  })
  if (hop.to === 'first') report.hops[2].status = 'RUNNING'
}

async function main(action) {
  const e = process.env,
    platform = process.platform,
    channel = e.G6_CHANNEL
  if (
    e.GITHUB_ACTIONS !== 'true' ||
    e.RUNNER_ENVIRONMENT !== 'github-hosted' ||
    e.GITHUB_EVENT_NAME !== 'workflow_dispatch' ||
    e.GITHUB_REPOSITORY !== 'seuput-inc/seuoffice' ||
    e.GITHUB_REF !== 'refs/heads/integration/tauri-first-candidate' ||
    !e.RUNNER_TEMP ||
    !['win32', 'darwin'].includes(platform) ||
    !['latest', 'stable'].includes(channel)
  )
    throw Error('Requires a disposable hosted candidate-branch dispatch')
  const root = join(e.RUNNER_TEMP, 'g6-qa'),
    output = join(e.RUNNER_TEMP, 'g6-result.json'),
    planPath = join(root, 'plan.json')
  let report = initialReport(platform, channel)
  try {
    if (action === 'prepare') {
      json(output, report)
      const [bridge, first, next] = targets(e)
      mkdirSync(root)
      const bridgeDir = join(root, 'bridge')
      const artifact = `${platform === 'darwin' ? 'seuoffice-mac-arm64' : 'seuoffice-windows-x64'}-migration-seam`
      const run = JSON.parse(
        execFileSync('gh', ['api', `repos/${e.GITHUB_REPOSITORY}/actions/runs/${bridge[0]}`], {
          encoding: 'utf8',
        }),
      )
      if (
        run.head_repository?.full_name !== e.GITHUB_REPOSITORY ||
        run.head_branch !== e.GITHUB_REF_NAME ||
        run.event !== 'workflow_dispatch' ||
        run.path !== '.github/workflows/seuoffice-desktop.yml' ||
        run.head_sha !== bridge[2] ||
        run.conclusion !== 'success' ||
        run.status !== 'completed'
      )
        throw Error('Unexpected bridge producer')
      execFileSync(
        'gh',
        [
          'run',
          'download',
          bridge[0],
          '--repo',
          e.GITHUB_REPOSITORY,
          '--name',
          artifact,
          '--dir',
          bridgeDir,
        ],
        { stdio: 'inherit' },
      )
      const br = await verifyBridge([platform, ...bridge, bridgeDir, 'loopback'])
      const payloadKind = platform === 'darwin' ? 'archive' : 'installer'
      const bridgeFiles = br.files.filter((f) => f.channel === channel)
      const plan = {
        channel,
        bridge: {
          version: br.version,
          installer: bridgeFiles.find((f) => f.kind === payloadKind).path,
        },
      }
      report.sources = { bridge: br }
      for (const [stage, args] of [
        ['first', first],
        ['next', next],
      ]) {
        const dir = join(root, `tauri-${args[0]}`)
        execFileSync(
          process.execPath,
          [
            join(import.meta.dirname, 'candidate.mjs'),
            platform,
            ...args,
            dir,
            stage,
            'loopback',
            channel,
          ],
          { stdio: 'inherit' },
        )
        const receipt = read(join(dir, `${stage}-receipt.json`))
        if (
          receipt.mode !== 'loopback' ||
          receipt.promotable !== false ||
          receipt.channel !== channel
        )
          throw Error('Not a loopback QA artifact')
        const files = receipt.channels
          .find((c) => c.channel === channel)
          .files.map((f) => ({ ...f, path: resolve(dir, f.relativePath) }))
        plan[stage] = { version: args[1], installer: receipt.file, files }
        report.sources[stage] = receipt
      }
      if (br.promotable !== false || br.feedUrl !== 'http://127.0.0.1:8765/')
        throw Error('Unexpected bridge feed')
      json(planPath, plan)
      report.preparation = 'VERIFIED'
    } else if (action === 'execute') {
      report = read(output)
      if (
        report.preparation !== 'VERIFIED' ||
        report.channel !== channel ||
        report.platform !== platform
      )
        throw Error('No matching verified plan')
      const plan = read(planPath),
        control = join(root, 'stage'),
        out = join(root, 'driver')
      report.execution = 'RUNNING'
      mkdirSync(out)
      writeFileSync(control, 'first')
      const feed = suffixFeed(plan, () => readFileSync(control, 'utf8').trim())
      feed.server.listen(8765, '127.0.0.1')
      await once(feed.server, 'listening')
      try {
        report.hops[1].status = 'RUNNING'
        json(output, report)
        const mac = platform === 'darwin'
        const script = join(import.meta.dirname, mac ? 'g6-mac.sh' : 'g6-windows.ps1')
        const child = spawn(
          mac ? '/bin/bash' : 'pwsh',
          mac ? [script] : ['-NoProfile', '-NonInteractive', '-File', script],
          {
            env: { ...e, G6_PLAN: planPath, G6_OUT: out, G6_FEED_CONTROL: control },
            stdio: ['ignore', 'pipe', 'inherit'],
          },
        )
        const completion = once(child, 'close')
        let invalid
        let blockedReason
        for await (const line of createInterface({ input: child.stdout, crlfDelay: Infinity })) {
          if (line.startsWith('G6_BLOCKED ')) {
            blockedReason = line.slice(11, 251)
            continue
          }
          if (!line.startsWith('G6_EVENT ')) {
            console.log(line)
            continue
          }
          try {
            recordArrival(report, plan, JSON.parse(line.slice(9)), feed.observed)
            json(output, report)
          } catch (err) {
            invalid = err
            child.kill()
          }
        }
        const [code] = await completion
        if (invalid) throw invalid
        if (code === 3) {
          report.execution = 'BLOCKED'
          throw Error(blockedReason || 'OS driver could not observe the update')
        }
        if (code !== 0 || report.hops[2].status !== 'PASSED')
          throw Error(`OS driver did not verify both updates (exit ${code})`)
        report.execution = 'PASSED'
      } finally {
        feed.server.closeAllConnections()
        feed.server.close()
      }
    } else throw Error('Expected prepare or execute')
  } catch (err) {
    if (action === 'prepare') report.preparation = 'FAILED'
    else if (report.execution === 'RUNNING') report.execution = 'FAILED'
    const running = report.hops.find((h) => h.status === 'RUNNING')
    if (running) {
      running.status = report.execution === 'BLOCKED' ? 'BLOCKED' : 'NOT_VERIFIED'
      running.reason = err.message
    }
    throw err
  } finally {
    json(output, report)
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) await main(process.argv[2])
