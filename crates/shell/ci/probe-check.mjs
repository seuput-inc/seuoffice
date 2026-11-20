#!/usr/bin/env node
// Reads what the hosted WebView probes recorded (src/probe/*, one folder per probe:
// <root>/<probe>/<probe>.jsonl, the artifacts beside it, the shell's log as shell.log, and for
// p10 the `--measure-dialog` JSON as dialog.json) and says, per assertion:
//
//   observed    the record shows what the design assumed (the evidence line is quoted)
//   violated    the record contradicts it (the evidence line is quoted)
//   unobserved  the record holds nothing that bears on it; a probe that did not run, or ended
//               early, leaves its assertions here. Never a pass.
//   info        a measurement with no verdict
//   n/a         the assertion is about the other OS
//
// Exit status: 1 when anything is violated, 2 when nothing is violated but something is
// unobserved, 0 otherwise.
//
//   node probe-check.mjs <root> [--probes p1,p2,...] [--json out.json] [--markdown out.md]
//                        [--p10-ratio 5]

import fs from 'node:fs'
import path from 'node:path'
import { decodePng, fillsNear, marker, measurePdf, pageSize } from './probe-media.mjs'

const argv = process.argv.slice(2)
const option = (name, fallback) => {
  const at = argv.indexOf(name)
  return at >= 0 && argv[at + 1] ? argv[at + 1] : fallback
}
const root = argv.find(
  (arg, index) => !arg.startsWith('--') && (index === 0 || !argv[index - 1].startsWith('--')),
)
if (!root) {
  console.error(
    'usage: probe-check.mjs <root> [--probes p1,p2] [--json out.json] [--markdown out.md] [--p10-ratio 5]',
  )
  process.exit(64)
}
const ALL = ['p1', 'p2', 'p3', 'p4', 'p5', 'p6', 'p7', 'p8', 'p9', 'p10']
const wanted = option('--probes', ALL.join(','))
  .split(',')
  .map((name) => name.trim())
  .filter(Boolean)
const P10_RATIO = Number(option('--p10-ratio', '5'))

// ------------------------------------------------------------------------------------ loading

function load(probe) {
  const dir = path.join(root, probe)
  if (probe === 'p10') {
    const file = path.join(dir, 'dialog.json')
    if (!fs.existsSync(file)) return { probe, dir, missing: true, recs: [] }
    try {
      const report = JSON.parse(fs.readFileSync(file, 'utf8'))
      return {
        probe,
        dir,
        recs: [],
        report,
        platform: report.environment?.os === 'macos' ? 'macos' : report.environment?.os,
      }
    } catch (error) {
      return {
        probe,
        dir,
        missing: true,
        recs: [],
        note: `dialog.json is not JSON: ${error.message}`,
      }
    }
  }
  const file = path.join(dir, `${probe}.jsonl`)
  if (!fs.existsSync(file)) return { probe, dir, missing: true, recs: [] }
  const recs = []
  let unreadable = 0
  fs.readFileSync(file, 'utf8')
    .split('\n')
    .forEach((text, index) => {
      if (!text.trim()) return
      try {
        const rec = JSON.parse(text)
        rec._line = index + 1
        recs.push(rec)
      } catch {
        unreadable += 1
      }
    })
  const logFile = path.join(dir, 'shell.log')
  return {
    probe,
    dir,
    recs,
    unreadable,
    log: fs.existsSync(logFile) ? fs.readFileSync(logFile, 'utf8') : undefined,
    platform: recs.find((rec) => rec.ev === 'probe.start')?.platform,
    ended: recs.find((rec) => rec.ev === 'probe.end'),
    skipped: recs.find((rec) => rec.ev === 'probe.skip'),
  }
}

// ----------------------------------------------------------------------------------- results

const short = (rec) => {
  const { _line, _probe, wall: _wall, href: _href, seq: _seq, ...rest } = rec
  const text = JSON.stringify(rest)
  return text.length > 300 ? `${text.slice(0, 297)}...` : text
}
const at = (rec) => `${rec._probe ?? ''}L${rec._line}: ${short(rec)}`
const result = (status, rec, note) => ({
  status,
  evidence: [rec ? at(rec) : undefined, note].filter(Boolean).join(' | '),
})
const ok = (rec, note) => result('observed', rec, note)
const bad = (rec, note) => result('violated', rec, note)
const none = (note) => ({ status: 'unobserved', evidence: note })
const info = (rec, note) => result('info', rec, note)

const rows = []
const assertion = (probe, id, text, source, check, platforms) =>
  rows.push({ probe, id, text, source, check, platforms })

const recsOf = (R, ev) => R.recs.filter((rec) => rec.ev === ev)
const firstOf = (R, ev, test = () => true) => R.recs.find((rec) => rec.ev === ev && test(rec))
const op = (R, name) => R.recs.filter((rec) => rec.ev === 'op' && rec.name === name).at(-1)
const nativeOf = (R, view) =>
  R.recs.filter((rec) => rec.src === 'native' && (!view || rec.view === view))
const kindOf = (rec) => rec.ev.replace(/^native\./, '')

// Combine per-item verdicts into one: violated if any, else observed if all, else unobserved.
function combine(items) {
  if (!items.length) return none('no item to judge')
  const worst =
    items.find((item) => item.status === 'violated') ??
    items.find((item) => item.status === 'unobserved')
  const best = worst ?? items[0]
  const lines = items.map(
    (item) =>
      `${item.status === 'observed' ? '+' : item.status === 'violated' ? '-' : '?'} ${item.label}`,
  )
  return {
    status: best.status,
    evidence: [best.evidence, lines.join('; ')].filter(Boolean).join(' || '),
  }
}
const item = (status, label, rec, note) => ({
  status,
  label: note ? `${label}: ${note}` : label,
  evidence: rec ? at(rec) : undefined,
})

// ------------------------------------------------------------------------------------ P1

const P1 = 'design 0.3 W6, 0.5 P1'
assertion(
  'p1',
  'P1.1',
  'the real preload bridge sendSync (synchronous XHR to seuoffice-ipc) is answered by the handler, 20 times',
  `${P1}; S2`,
  (R) => {
    const reals = recsOf(R, 'p1.real')
    if (!reals.length) return none('no p1.real record: the page did not reach the sync call')
    const failed = reals.find((rec) => !rec.ok)
    if (failed) return bad(failed, `sendSync threw ${failed.name}: ${failed.error}`)
    const timing = firstOf(R, 'p1.real.timing')
    if (timing && timing.n !== 20) return bad(timing, `${timing.n} of 20 calls returned`)
    return ok(reals[0], `answer ${JSON.stringify(reals[0].answer)}`)
  },
)
assertion(
  'p1',
  'P1.2',
  'a raw synchronous XHR with the x-seuoffice-doc custom header completes (CORS preflight accepted) and its 403 body is readable',
  'S2: Access-Control-Allow-Headers: * accepted for a custom header',
  (R) => {
    const raw = firstOf(R, 'p1.raw', (rec) => rec.name === 'post-0')
    if (!raw) return none('no p1.raw post-0 record')
    if (!raw.ok) return bad(raw, `the XHR failed: ${raw.error}`)
    return raw.status === 403 && raw.length > 0
      ? ok(raw)
      : bad(raw, `expected a readable 403, got status ${raw.status} length ${raw.length}`)
  },
)
assertion(
  'p1',
  'P1.3',
  'the handler receives the custom header on the POST (the nonce channel)',
  'S2; design 1.4',
  (R) => {
    const posts = R.recs.filter(
      (rec) =>
        rec.ev === 'scheme.request' &&
        rec.scheme === 'seuoffice-ipc' &&
        rec.method === 'POST' &&
        rec.body_len > 0,
    )
    if (!posts.length) return none('the handler saw no POST with a body')
    const without = posts.find((rec) => rec.doc_header_len !== 32 && rec.body_len === 4096)
    const withHeader = posts.find((rec) => rec.doc_header_len === 32)
    if (withHeader) return ok(withHeader)
    return bad(without ?? posts[0], 'no POST carried the 32-character x-seuoffice-doc header')
  },
)
for (const [id, size] of [
  ['P1.4', 4096],
  ['P1.5', 1048576],
]) {
  assertion(
    'p1',
    id,
    `a ${size}-byte POST body reaches the handler whole`,
    'wry wkwebview url_scheme_handler body/stream read; design 1.4',
    (R) => {
      const asked = firstOf(R, 'p1.raw', (rec) => rec.name === `post-${size}`)
      const seen = R.recs.filter(
        (rec) =>
          rec.ev === 'scheme.request' && rec.scheme === 'seuoffice-ipc' && rec.method === 'POST',
      )
      if (!asked) return none(`the page did not send the ${size}-byte request`)
      const exact = seen.find((rec) => rec.body_len === size)
      if (exact) return ok(exact)
      return bad(
        seen.find((rec) => rec.body_len > 0 && rec.body_len !== 4096 && rec.body_len !== 0) ??
          asked,
        `no POST arrived with body_len ${size} (saw ${seen.map((rec) => rec.body_len).join(',') || 'none'})`,
      )
    },
  )
}
assertion(
  'p1',
  'P1.6',
  'a GET with the custom header (outbox pull shape) also reaches the handler and is answered',
  'S2; design 1.4',
  (R) => {
    const raw = firstOf(R, 'p1.raw', (rec) => rec.name === 'get-outbox')
    if (!raw) return none('no p1.raw get-outbox record')
    return raw.ok && raw.status === 403
      ? ok(raw)
      : bad(raw, 'expected a readable 403 from the outbox path with a made-up nonce')
  },
)
assertion(
  'p1',
  'P1.7',
  'the sync URL has the per-OS shape the bridge builds (macOS seuoffice-ipc://localhost/, Windows http://seuoffice-ipc.localhost/)',
  'design 0.5 P1; app_scheme header',
  (R) => {
    const env = firstOf(R, 'p1.env')
    if (!env) return none('no p1.env record')
    const expected =
      R.platform === 'windows' ? 'http://seuoffice-ipc.localhost/' : 'seuoffice-ipc://localhost/'
    return typeof env.ipcUrl === 'string' && env.ipcUrl.startsWith(expected)
      ? ok(env)
      : bad(env, `expected a URL starting with ${expected}`)
  },
)
assertion(
  'p1',
  'P1.8',
  'preflight requests the handler saw (OPTIONS), and the sync call timing',
  P1,
  (R) => {
    const options = R.recs.filter(
      (rec) =>
        rec.ev === 'scheme.request' && rec.scheme === 'seuoffice-ipc' && rec.method === 'OPTIONS',
    )
    const timing = firstOf(R, 'p1.real.timing')
    return info(timing, `${options.length} OPTIONS requests seen by the handler`)
  },
)
assertion(
  'p1',
  'P1.9',
  'window.prompt() returns at once with null where no prompt panel is implemented (the fallback would need a WKUIDelegate wrapper)',
  'design 0.5 P1 fallback; wry has only an open-panel delegate',
  (R) => {
    const prompt = firstOf(R, 'p1.prompt')
    if (!prompt) return none('no p1.prompt record')
    return prompt.value === null && prompt.ms < 2000
      ? ok(prompt)
      : bad(prompt, 'prompt() did not return null at once')
  },
  ['macos'],
)

// ------------------------------------------------------------------------------------ P2

const P2_VIEW = 'probe-p2'
const seq2 = (R) => nativeOf(R, P2_VIEW)
assertion(
  'p2',
  'P2.1',
  'every DocumentRegistered is preceded by the DocumentCommitted of the same document',
  'design 1.4, W7; S2 (register binds to the committed document)',
  (R) => {
    const events = seq2(R)
    const registered = events.filter((rec) => kindOf(rec) === 'DocumentRegistered')
    if (!registered.length) return none('no DocumentRegistered event')
    const early = registered.find(
      (rec) =>
        !events.some(
          (other) =>
            kindOf(other) === 'DocumentCommitted' &&
            other.document === rec.document &&
            other._line < rec._line,
        ),
    )
    return early
      ? bad(early, 'registered before (or without) its commit')
      : ok(registered[0], `${registered.length} registrations, each after its commit`)
  },
)
assertion(
  'p2',
  'P2.2',
  'every cross-document NavigationStarted is preceded by the DocumentRevoked(Navigation) of the document that was current',
  'design 1.5 delivery order; W7 (revocation at on_navigation admission)',
  (R) => {
    const events = seq2(R)
    let current
    let judged = 0
    for (const rec of events) {
      const kind = kindOf(rec)
      if (kind === 'DocumentCommitted') current = { document: rec.document, revoked: false }
      else if (kind === 'DocumentRevoked' && current && rec.document === current.document)
        current.revoked = true
      else if (kind === 'NavigationStarted' && !rec.same_document && current) {
        judged += 1
        if (!current.revoked)
          return bad(
            rec,
            `document ${current.document} was still live when this navigation started`,
          )
        current = undefined
      }
    }
    return judged
      ? ok(
          events.find((rec) => kindOf(rec) === 'NavigationStarted'),
          `${judged} navigations checked`,
        )
      : none('no cross-document navigation after a committed document')
  },
)
assertion(
  'p2',
  'P2.3',
  'NavigationStarted (on_navigation) precedes the DocumentCommitted of the same navigation',
  'design 0.5 P2: on_navigation runs before commit on both OSes',
  (R) => {
    const events = seq2(R)
    const commits = events.filter((rec) => kindOf(rec) === 'DocumentCommitted')
    if (!commits.length) return none('no DocumentCommitted event')
    const late = commits.find(
      (rec) =>
        !events.some(
          (other) =>
            kindOf(other) === 'NavigationStarted' &&
            other.navigation === rec.navigation &&
            other._line < rec._line,
        ),
    )
    return late
      ? bad(late, 'committed with no earlier NavigationStarted of its navigation')
      : ok(commits[0], `${commits.length} commits, each after its NavigationStarted`)
  },
)
assertion(
  'p2',
  'P2.4',
  'a fragment or pushState navigation is reported as same-document and neither revokes nor commits a document',
  'design W7: fragment-only and document.open() are not revocations',
  (R) => {
    const events = seq2(R)
    const ready = events.find((rec) => kindOf(rec) === 'DocumentReady')
    const revoke = events.find(
      (rec) => kindOf(rec) === 'DocumentRevoked' && ready && rec._line > ready._line,
    )
    if (!ready || !revoke) return none('no ready document that was later revoked by a navigation')
    // the revoke belongs to the navigation that leaves the page: the next NavigationStarted
    const leave = events.find(
      (rec) => kindOf(rec) === 'NavigationStarted' && rec._line > revoke._line,
    )
    if (leave?.same_document)
      return bad(leave, 'a same-document navigation revoked the document before it')
    const between = events.filter((rec) => rec._line > ready._line && rec._line < revoke._line)
    const same = between.filter(
      (rec) =>
        (kindOf(rec) === 'NavigationStarted' && rec.same_document) ||
        kindOf(rec) === 'AddressChanged',
    )
    const harmful = between.find((rec) => kindOf(rec) === 'DocumentCommitted')
    if (harmful) return bad(harmful, 'a same-document navigation committed a new document')
    return same.length
      ? ok(
          same[0],
          `${same.length} same-document events, no revoke or commit before the page was left`,
        )
      : none('the fragment and pushState navigations produced no event')
  },
)
assertion(
  'p2',
  'P2.5',
  'every committed page document (first load, cross-document, reload) ends up registered by its own bridge',
  'S2, S4: registration after commit; design 1.4',
  (R) => {
    const events = seq2(R)
    const pages = events.filter(
      (rec) => kindOf(rec) === 'DocumentCommitted' && !String(rec.url).startsWith('about:'),
    )
    if (pages.length < 3)
      return none(
        `only ${pages.length} page documents committed; the sequence needs at least 3 (a, b, b reloaded)`,
      )
    const lost = pages.find(
      (rec) =>
        !events.some(
          (other) => kindOf(other) === 'DocumentRegistered' && other.document === rec.document,
        ),
    )
    return lost
      ? bad(lost, 'this document was never registered')
      : ok(pages.at(-1), `${pages.length} documents, all registered`)
  },
)
assertion(
  'p2',
  'P2.6',
  'a restore from the page cache (pageshow persisted) does not conflict-revoke a document: no DocumentRevoked(Conflict) in the run',
  'design W7; F0 follow-up 6; S2 inference',
  (R) => {
    const conflict = seq2(R).find(
      (rec) => kindOf(rec) === 'DocumentRevoked' && rec.reason === 'Conflict',
    )
    const back = recsOf(R, 'p2.pageshow').find((rec) => rec.phase === 'back')
    const how = back
      ? `traversal back ${back.persisted ? 'restored from the page cache' : 'reloaded the page (page cache not used on this OS)'}`
      : 'traversal back did not report'
    if (conflict) return bad(conflict, how)
    return back ? ok(back, how) : none(how)
  },
)
assertion(
  'p2',
  'P2.7',
  'the browser ends the run with a registered, live document',
  'S4: registration survives the whole sequence',
  (R) => {
    const done = firstOf(R, 'p2.final')
    if (!done) return none('no p2.final record')
    return done.registered ? ok(done) : bad(done, 'no registered document at the end')
  },
)
assertion(
  'p2',
  'P2.8',
  'DocumentContext::evaluate run at the moment of DocumentRegistered succeeds (the page already knows its registration)',
  'S2 inference: Registered before Ready may race __seuofficeCheck',
  (R) => {
    const attempts = recsOf(R, 'p2.eval.at_registration')
    if (!attempts.length) return none('no evaluation was attempted at registration')
    const failed = attempts.find((rec) => !rec.ok)
    return failed
      ? bad(failed, `${failed.error}`)
      : ok(attempts[0], `${attempts.length} of ${attempts.length} succeeded`)
  },
)
assertion(
  'p2',
  'P2.9',
  'DocumentContext::evaluate of the final document answers',
  'S2; F0 follow-up 5 (CSP and the evaluation wrapper)',
  (R) => {
    const done = firstOf(R, 'p2.final')
    if (!done?.state) return none('no registered document to evaluate in')
    return done.state.evaluate ? ok(done) : bad(done, `no answer: ${done.state.evaluate_error}`)
  },
)
assertion(
  'p2',
  'P2.10',
  'a cache-ignoring reload (reload_ignoring_cache) is dispatched and the reloaded document registers again',
  'S1w: reload_ignoring_cache returns Ok once dispatched; S1m reloadFromOrigin; design 2.6',
  (R) => {
    const rec = firstOf(R, 'p2.reload_ignoring_cache')
    if (!rec) return none('no p2.reload_ignoring_cache record')
    return rec.dispatched && rec.registered_after
      ? ok(rec)
      : bad(rec, rec.error ?? 'no DocumentRegistered after the reload')
  },
)

// ------------------------------------------------------------------------------------ P3

const P3_APPS = ['app-shell', 'app-docs', 'app-sheets', 'app-engine']
const P3_SCHEMES = [
  'seuoffice-docx-media',
  'md-asset',
  'html-asset',
  'seuoffice-slide-media',
  'drawio-app',
]
const p3Targets = (R) => {
  const names = new Map(recsOf(R, 'p3.target').map((rec) => [rec.view, rec.name]))
  return recsOf(R, 'p3.context').map((rec) => ({ name: names.get(rec.view) ?? rec.view, rec }))
}
const p3Rule = (id, text, source, test, filter = () => true) =>
  assertion('p3', id, text, source, (R) => {
    const targets = p3Targets(R).filter(({ name }) => filter(name))
    if (!targets.length) return none('no p3.context record')
    return combine(
      targets.map(({ name, rec }) => {
        const verdict = test(rec, name)
        return item(verdict.ok ? 'observed' : 'violated', `${name}`, rec, verdict.note)
      }),
    )
  })
const P3 = 'design 0.5 P3'
p3Rule(
  'P3.1',
  'every scheme document is a secure context',
  `${P3}; native_custom_schemes in CEF`,
  (rec) => ({ ok: rec.secure === true, note: `isSecureContext=${rec.secure}` }),
)
p3Rule('P3.2', 'crypto.subtle exists in every scheme document', P3, (rec) => ({
  ok: rec.subtle === 'object',
  note: `typeof crypto.subtle=${rec.subtle}`,
}))
p3Rule(
  'P3.3',
  'streaming WebAssembly (instantiateStreaming over the scheme) works in every scheme document',
  P3,
  (rec) => ({ ok: rec.wasm === 'ok', note: rec.wasm }),
)
p3Rule(
  'P3.4',
  'the engine host (served COOP/COEP) is cross-origin isolated with SharedArrayBuffer',
  `${P3}; app_scheme.rs engine host headers`,
  (rec) => ({
    ok: rec.isolated === true && rec.sab === 'function',
    note: `crossOriginIsolated=${rec.isolated} SharedArrayBuffer=${rec.sab}`,
  }),
  (name) => name === 'app-engine',
)
p3Rule('P3.5', 'localStorage and IndexedDB work in every scheme document', P3, (rec) => ({
  ok: rec.lsAfter === rec.origin && !rec.lsError && rec.idb === 'ok',
  note: `localStorage=${rec.lsError ?? rec.lsAfter === rec.origin} indexedDB=${rec.idb}`,
}))
p3Rule(
  'P3.6',
  'localStorage is separate per host: the second app host does not see the first one’s value',
  'app_scheme.rs header: one origin per host',
  (rec) => ({ ok: rec.lsBefore === null, note: `lsBefore=${JSON.stringify(rec.lsBefore)}` }),
  (name) => name === 'app-sheets',
)
p3Rule(
  'P3.7',
  'each document has its own origin in the per-OS shape (macOS scheme://localhost, Windows http://scheme.localhost)',
  `${P3}; app_scheme.rs`,
  (rec, name) => {
    const scheme = P3_APPS.includes(name)
      ? `genoffice-app-${name.replace('app-', '').replace('engine', 'engine-host')}`
      : name
    const expected =
      rec.protocol === 'http:' || rec.protocol === 'https:'
        ? `http://${scheme}.localhost`
        : `${scheme}://localhost`
    return { ok: rec.origin === expected, note: `origin=${rec.origin} expected=${expected}` }
  },
)
p3Rule(
  'P3.8',
  'a Docs page reads from the docx media scheme with CORS (fetch) and loads its images (img)',
  'apps/docs CSP names seuoffice-docx-media in img-src and connect-src',
  (rec) => ({
    ok: rec.fetchMedia?.status === 200 && rec.imgMedia?.ok === true,
    note: `fetch=${JSON.stringify(rec.fetchMedia)} img=${JSON.stringify(rec.imgMedia)}`,
  }),
  (name) => name === 'app-docs',
)
assertion(
  'p3',
  'P3.9',
  'the HWP shell page reads the studio child frame’s location and calls its window function (both genoffice-app-hwp)',
  'design 3.5; S7 inference',
  (R) => {
    const rec = firstOf(R, 'p3.hwp')
    if (!rec) return none('no p3.hwp record')
    return rec.ready && rec.keyAnswer === true && String(rec.childHref).includes('/studio/')
      ? ok(rec)
      : bad(rec, rec.error ?? 'the child frame was not reachable')
  },
)
assertion('p3', 'P3.10', 'every target document reported', P3, (R) => {
  const ends = recsOf(R, 'p3.target.end')
  const missed = ends.find((rec) => !rec.page_done) ?? recsOf(R, 'p3.target.failed')[0]
  const names = new Set(recsOf(R, 'p3.target').map((rec) => rec.name))
  const expected = [...P3_APPS, ...P3_SCHEMES, 'app-hwp']
  const absent = expected.filter((name) => !names.has(name))
  if (missed) return bad(missed, 'a target did not report')
  if (absent.length) return none(`targets never opened: ${absent.join(', ')}`)
  return ok(ends.at(-1), `${ends.length} targets`)
})

// ------------------------------------------------------------------------------------ P4

const pdfCache = new Map()
function pdfOf(R, file) {
  const key = `${R.dir}/${file}`
  if (!pdfCache.has(key)) {
    try {
      pdfCache.set(key, { measured: measurePdf(fs.readFileSync(path.join(R.dir, file))) })
    } catch (error) {
      pdfCache.set(key, { error: error.message })
    }
  }
  return pdfCache.get(key)
}
// [case record, its result record] for each print run
const runs = (R) =>
  recsOf(R, 'p4.case').map((rec) => ({
    rec,
    outcome: R.recs.find(
      (other) =>
        other.ev === 'p4.result' && other.case === rec.case && other.variant === rec.variant,
    ),
  }))
function perRun(R, select, judge) {
  const chosen = runs(R).filter(({ rec }) => select(rec))
  if (!chosen.length) return none('no matching print case in the record')
  return combine(
    chosen.map(({ rec, outcome }) => {
      const label = `${rec.case}/${rec.variant}`
      if (!outcome) return item('unobserved', label, rec, 'no result record')
      if (!outcome.ok)
        return item('unobserved', label, outcome, `the print failed: ${outcome.error}`)
      const pdf = pdfOf(R, rec.file)
      if (pdf.error)
        return item('unobserved', label, outcome, `the PDF could not be read: ${pdf.error}`)
      const verdict = judge(rec, pdf.measured)
      return item(
        verdict.ok === undefined ? 'unobserved' : verdict.ok ? 'observed' : 'violated',
        label,
        outcome,
        verdict.note,
      )
    }),
  )
}
const near = (value, target, tolerance) =>
  typeof value === 'number' && Math.abs(value - target) <= tolerance
const fmt = (value) => (typeof value === 'number' ? value.toFixed(1) : String(value))
const P4 = 'design 3.2, 0.5 P4'
assertion(
  'p4',
  'P4.1',
  'the PDF has the page count the corpus defines (explicit page breaks)',
  `${P4}; S1m page-break layout`,
  (R) =>
    perRun(
      R,
      (rec) => rec.expect.pages !== undefined,
      (rec, pdf) => ({
        ok: pdf.pages.length === rec.expect.pages,
        note: `${pdf.pages.length} pages, expected ${rec.expect.pages}`,
      }),
    ),
)
assertion(
  'p4',
  'P4.2',
  'paper size and landscape are applied (A4 portrait 595x842, landscape 842x595)',
  `${P4}; S1m: paperSize is oriented`,
  (R) =>
    perRun(
      R,
      (rec) => rec.expect.media && rec.case !== 'css-page-size',
      (rec, pdf) => {
        const sizes = pdf.pages.map(pageSize)
        const wrong = sizes.find(
          (size) =>
            !size ||
            !near(size[0], rec.expect.media[0], 2) ||
            !near(size[1], rec.expect.media[1], 2),
        )
        return {
          ok: !wrong,
          note: `${sizes.map((size) => size?.map(fmt).join('x')).join(', ')} expected ${rec.expect.media.join('x')}`,
        }
      },
    ),
)
assertion(
  'p4',
  'P4.3',
  'CSS @page size is honoured with preferCSSPageSize (A4 landscape although the paper asked is portrait)',
  'S1m: prefer_css_page_size is not applied explicitly',
  (R) =>
    perRun(
      R,
      (rec) => rec.case === 'css-page-size',
      (rec, pdf) => {
        const size = pageSize(pdf.pages[0])
        return {
          ok:
            !!size &&
            near(size[0], rec.expect.media[0], 2) &&
            near(size[1], rec.expect.media[1], 2),
          note: `${size?.map(fmt).join('x')} expected ${rec.expect.media.join('x')}`,
        }
      },
    ),
)
assertion(
  'p4',
  'P4.4',
  'margins: the content box starts at the asked margin (spec margins, and the CSS @page margin with preferCSSPageSize)',
  `${P4}; S1m: margins and scalingFactor unconfirmed`,
  (R) =>
    perRun(
      R,
      (rec) => rec.expect.marker?.left !== undefined,
      (rec, pdf) => {
        const seen = pdf.pages.map((page) => marker(page))
        if (seen.some((entry) => !entry))
          return {
            ok: undefined,
            note: 'no marker square found on some page (background not painted or file unreadable)',
          }
        const tolerance = rec.expect.tolerance ?? 3
        const wrong = seen.find(
          (entry) =>
            !near(entry.left, rec.expect.marker.left, tolerance) ||
            !near(entry.top, rec.expect.marker.top, tolerance),
        )
        return {
          ok: !wrong,
          note: `left/top ${seen.map((entry) => `${fmt(entry.left)}/${fmt(entry.top)}`).join(', ')} expected ${rec.expect.marker.left}/${rec.expect.marker.top} ±${tolerance}`,
        }
      },
    ),
)
assertion(
  'p4',
  'P4.5',
  'scale 0.5 halves the content (36pt marker becomes 18pt) and leaves the margin',
  `${P4}; S1m: scalingFactor`,
  (R) =>
    perRun(
      R,
      (rec) => rec.case === 'scale-half',
      (rec, pdf) => {
        const seen = pdf.pages.map((page) => marker(page))
        if (seen.some((entry) => !entry)) return { ok: undefined, note: 'no marker square found' }
        return {
          ok: seen.every((entry) =>
            near(entry.width, rec.expect.marker.size, rec.expect.tolerance),
          ),
          note: `marker width ${seen.map((entry) => fmt(entry.width)).join(', ')} expected ${rec.expect.marker.size}`,
        }
      },
    ),
)
assertion(
  'p4',
  'P4.6',
  'backgrounds are painted with print_background and absent without',
  `${P4}; S1m: preferences copy reaches the live page`,
  (R) =>
    perRun(
      R,
      (rec) => rec.expect.fill,
      (rec, pdf) => {
        const present = pdf.pages.every((page) => fillsNear(page, rec.expect.fill.rgb).length > 0)
        const absent = pdf.pages.every((page) => fillsNear(page, rec.expect.fill.rgb).length === 0)
        return {
          ok: rec.expect.fill.present ? present : absent,
          note: `the blue background is ${present ? 'on every page' : absent ? 'on no page' : 'on some pages'}, expected ${rec.expect.fill.present ? 'present' : 'absent'}`,
        }
      },
    ),
)
assertion(
  'p4',
  'P4.7',
  'pageRanges keeps exactly the asked pages, in order (identified by their swatch colors)',
  `${P4}; design 3.2: lopdf slice of the full output`,
  (R) =>
    perRun(
      R,
      (rec) => rec.expect.swatches,
      (rec, pdf) => {
        const seen = pdf.pages.map((page) => {
          const fill = page.fills.find(
            ({ color }) =>
              color[1] <= 6 &&
              color[2] <= 6 &&
              color[0] >= 34 &&
              color[0] <= 206 &&
              Math.abs((color[0] % 40 > 20 ? color[0] - 40 : color[0]) % 40) <= 6,
          )
          return fill ? Math.round(fill.color[0] / 40) : undefined
        })
        return {
          ok:
            seen.length === rec.expect.swatches.length &&
            seen.every((value, index) => value === rec.expect.swatches[index]),
          note: `pages ${seen.join(',')} expected ${rec.expect.swatches.join(',')}`,
        }
      },
    ),
)
assertion(
  'p4',
  'P4.8',
  'header and footer templates with Hangul title and page numbers are applied (their characters appear in the PDF fonts, which the same page without templates lacks)',
  'MacHeaderFooter; design D1',
  (R) => {
    const chosen = runs(R).find(({ rec }) => rec.case === 'header-footer')
    const base = runs(R).find(({ rec }) => rec.case === 'docs-a4' && rec.variant === 'hidden')
    if (!chosen?.outcome?.ok)
      return none(
        `the header-footer print ${chosen?.outcome ? `failed: ${chosen.outcome.error}` : 'did not run'}`,
      )
    const pdf = pdfOf(R, chosen.rec.file)
    if (pdf.error) return none(`the PDF could not be read: ${pdf.error}`)
    const has = chosen.rec.expect.unicode.map((code) => pdf.measured.unicode.has(code))
    if (!pdf.measured.unicode.size) return none('the PDF carries no ToUnicode maps to read')
    const baseline = base?.outcome?.ok ? pdfOf(R, base.rec.file).measured?.unicode : undefined
    const control = baseline ? chosen.rec.expect.unicode.some((code) => baseline.has(code)) : false
    if (control) return none('the control page (no templates) already holds the header characters')
    return has.every(Boolean)
      ? ok(
          chosen.outcome,
          `U+${chosen.rec.expect.unicode.map((code) => code.toString(16)).join(', U+')} present`,
        )
      : bad(chosen.outcome, 'the header characters are missing from the PDF fonts')
  },
)
assertion(
  'p4',
  'P4.9',
  'a hidden (never shown) window prints the same page count as a shown one',
  'S6 inference: a hidden print window may need to be shown',
  (R) =>
    perRun(
      R,
      (rec) => rec.case === 'docs-a4',
      (rec, pdf) => ({
        ok: pdf.pages.length === rec.expect.pages,
        note: `${pdf.pages.length} pages`,
      }),
    ),
)
assertion('p4', 'P4.10', 'every print run produced a PDF', P4, (R) => {
  const all = recsOf(R, 'p4.result')
  if (!all.length) return none('no print result')
  const failed = all.find((rec) => !rec.ok)
  return failed ? bad(failed) : ok(all[0], `${all.length} runs`)
})

// ---------------------------------------------------------------------------------- P5 / P6

function pngOf(R, name) {
  const done = op(R, `capture.${name}`)
  if (!done) return { none: `no capture.${name} operation` }
  if (!done.ok) return { rec: done, error: `capture.${name} failed: ${done.error}` }
  try {
    return { rec: done, png: decodePng(fs.readFileSync(path.join(R.dir, done.value.file))) }
  } catch (error) {
    return { rec: done, error: `the PNG could not be read: ${error.message}` }
  }
}
const pageOpsRule = (id, text, source, check, platforms) => {
  for (const probe of ['p5', 'p6'])
    assertion(
      probe,
      id.replace('PO', probe.toUpperCase()),
      text,
      source,
      check,
      [probe === 'p5' ? 'macos' : 'windows', ...(platforms ?? [])].slice(0, 1),
    )
}
const value = (R, name) => op(R, name)
const need = (rec, name) => (rec ? undefined : none(`no ${name} operation recorded`))
const SRC_E = 'design 0.5 P5/P6; S1m, S1w, F0'
pageOpsRule('PO.1', 'evaluate of a plain expression returns its value', SRC_E, (R) => {
  const rec = value(R, 'eval.sync')
  return need(rec, 'eval.sync') ?? (rec.ok && rec.value === 2 ? ok(rec) : bad(rec, 'expected 2'))
})
pageOpsRule(
  'PO.2',
  'evaluate with await_promise returns the settled value of a promise (callAsyncJavaScript / Runtime.evaluate awaitPromise)',
  SRC_E,
  (R) => {
    const rec = value(R, 'eval.promise')
    return (
      need(rec, 'eval.promise') ??
      (rec.ok && rec.value?.a === 1 ? ok(rec) : bad(rec, 'expected {a:1}'))
    )
  },
)
pageOpsRule(
  'PO.3',
  'a rejected promise is an error that carries the page’s message',
  SRC_E,
  (R) => {
    const rec = value(R, 'eval.reject')
    return (
      need(rec, 'eval.reject') ??
      (!rec.ok && /boom/.test(rec.error) ? ok(rec) : bad(rec, 'expected an error mentioning boom'))
    )
  },
)
pageOpsRule('PO.4', 'a 1 MiB string result comes back whole', SRC_E, (R) => {
  const rec = value(R, 'eval.big')
  return (
    need(rec, 'eval.big') ??
    (rec.ok && typeof rec.value === 'string' && rec.value.length === 1048576
      ? ok(rec)
      : bad(rec, 'expected a 1048576-character string'))
  )
})
pageOpsRule(
  'PO.5',
  'a statement list is refused on macOS (SyntaxError) and accepted on Windows',
  'F0: evaluate takes one expression; Windows tolerates more',
  (R) => {
    const rec = value(R, 'eval.statement-list')
    if (need(rec, 'eval.statement-list')) return need(rec, 'eval.statement-list')
    return R.platform === 'macos'
      ? !rec.ok
        ? ok(rec)
        : bad(rec, 'macOS accepted a statement list')
      : rec.ok
        ? ok(rec)
        : bad(rec, 'Windows refused a statement list')
  },
)
pageOpsRule(
  'PO.6',
  'run_script runs a classic script with a trailing semicolon, for its effect',
  'F0: run_script',
  (R) => {
    const set = value(R, 'run_script.set')
    const read = value(R, 'run_script.read')
    if (!set || !read) return none('run_script operations missing')
    return set.ok && read.ok && read.value === 7
      ? ok(read)
      : bad(set.ok ? read : set, 'the script did not run')
  },
)
pageOpsRule(
  'PO.7',
  'a viewport capture at scale 1 is css-pixel sized (width = innerWidth, height = innerHeight)',
  'S1m: snapshot size = snapshotWidth x backingScaleFactor',
  (R) => {
    const shot = pngOf(R, 'viewport')
    const metrics = value(R, 'eval.metrics')
    if (shot.none || !metrics?.ok) return none(shot.none ?? 'no page metrics')
    if (shot.error) return none(shot.error)
    const [width, height, ratio] = metrics.value
    return near(shot.png.width, width, 1) && near(shot.png.height, height, 1)
      ? ok(
          shot.rec,
          `${shot.png.width}x${shot.png.height} for a ${width}x${height} viewport, devicePixelRatio ${ratio}`,
        )
      : bad(
          shot.rec,
          `${shot.png.width}x${shot.png.height} for a ${width}x${height} viewport, devicePixelRatio ${ratio}`,
        )
  },
)
pageOpsRule(
  'PO.8',
  'a full-page capture is as tall as the page (3000 css px)',
  'S1m: resize to scrollHeight; S1w: captureBeyondViewport',
  (R) => {
    const shot = pngOf(R, 'fullpage')
    const metrics = value(R, 'eval.metrics')
    if (shot.none || !metrics?.ok) return none(shot.none ?? 'no page metrics')
    if (shot.error) return none(shot.error)
    const [width, , , scrollHeight] = metrics.value
    return near(shot.png.height, scrollHeight, 1) && near(shot.png.width, width, 1)
      ? ok(shot.rec, `${shot.png.width}x${shot.png.height}`)
      : bad(shot.rec, `${shot.png.width}x${shot.png.height}, page is ${width}x${scrollHeight}`)
  },
)
pageOpsRule(
  'PO.9',
  'a clip capture is the clip’s size at scale 1 (200x100) and double at scale 2 (400x200)',
  'S1m: snapshotWidth; design 3.1 (CaptureSpec scale)',
  (R) => {
    const one = pngOf(R, 'clip-box')
    const two = pngOf(R, 'clip-box-2x')
    if (one.none || two.none) return none(one.none ?? two.none)
    if (one.error || two.error) return none(one.error ?? two.error)
    const good =
      one.png.width === 200 &&
      one.png.height === 100 &&
      two.png.width === 400 &&
      two.png.height === 200
    return good
      ? ok(one.rec, `${one.png.width}x${one.png.height} and ${two.png.width}x${two.png.height}`)
      : bad(
          one.rec,
          `${one.png.width}x${one.png.height} at scale 1, ${two.png.width}x${two.png.height} at scale 2`,
        )
  },
)
pageOpsRule(
  'PO.10',
  'clip coordinates are top-left based (the clip’s top-left pixel is the green marker, its bottom-right the blue one)',
  'S1m: WKWebView is flipped',
  (R) => {
    const shot = pngOf(R, 'clip-box')
    if (shot.none) return none(shot.none)
    if (shot.error) return none(shot.error)
    const { png } = shot
    const topLeft = png.pixel(2, 2)
    const bottomRight = png.pixel(png.width - 3, png.height - 3)
    const green = topLeft[1] > 200 && topLeft[0] < 60 && topLeft[2] < 60
    const blue = bottomRight[2] > 200 && bottomRight[0] < 60 && bottomRight[1] < 60
    return green && blue
      ? ok(shot.rec, `top-left ${topLeft}, bottom-right ${bottomRight}`)
      : bad(shot.rec, `top-left ${topLeft}, bottom-right ${bottomRight}`)
  },
)
pageOpsRule(
  'PO.11',
  'transparent: true leaves the page background transparent (alpha 0); the default capture is opaque',
  'S1m: _setDrawsBackground; design P5',
  (R) => {
    const clear = pngOf(R, 'corner-transparent')
    const solid = pngOf(R, 'corner-opaque')
    if (clear.none || solid.none) return none(clear.none ?? solid.none)
    if (clear.error || solid.error) return none(clear.error ?? solid.error)
    const a = clear.png.pixel(5, 5)
    const b = solid.png.pixel(5, 5)
    return a[3] === 0 && b[3] === 255
      ? ok(clear.rec, `transparent ${a}, opaque ${b}`)
      : bad(clear.rec, `transparent ${a}, opaque ${b}`)
  },
)
pageOpsRule(
  'PO.12',
  'a hidden window captures the same image as a shown one (clip size, markers, full height)',
  'S1m: a hidden window renders after orderFrontRegardless at alpha 0',
  (R) => {
    const clip = pngOf(R, 'hidden-clip-box')
    const full = pngOf(R, 'hidden-fullpage')
    if (clip.none || full.none) return none(clip.none ?? full.none)
    if (clip.error || full.error) return bad(clip.rec ?? full.rec, clip.error ?? full.error)
    const topLeft = clip.png.pixel(2, 2)
    const good =
      clip.png.width === 200 &&
      clip.png.height === 100 &&
      topLeft[1] > 200 &&
      topLeft[0] < 60 &&
      near(full.png.height, 3000, 1)
    return good
      ? ok(
          clip.rec,
          `${clip.png.width}x${clip.png.height}, full ${full.png.width}x${full.png.height}`,
        )
      : bad(
          clip.rec,
          `clip ${clip.png.width}x${clip.png.height} top-left ${topLeft}, full ${full.png.width}x${full.png.height}`,
        )
  },
)
pageOpsRule(
  'PO.13',
  'set_viewport gives the page the asked layout size (800x600)',
  'design 3.4; S1m offscreen window; S1w Emulation.setDeviceMetricsOverride',
  (R) => {
    const set = value(R, 'viewport.set')
    const metrics = value(R, 'viewport.metrics')
    if (!set || !metrics) return none('viewport operations missing')
    if (!set.ok) return bad(set, set.error)
    return metrics.ok && metrics.value[0] === 800 && metrics.value[1] === 600
      ? ok(metrics, `devicePixelRatio ${metrics.value[2]}`)
      : bad(metrics, 'expected innerWidth 800 and innerHeight 600')
  },
)
pageOpsRule(
  'PO.14',
  'with page scripts off the page’s script did not run, but evaluate still answers',
  'S1m: allowsContentJavaScript=false still allows callAsyncJavaScript; S8',
  (R) => {
    const flag = value(R, 'nojs.flag')
    const sync = value(R, 'nojs.sync')
    if (!flag || !sync) return none('scripts-off operations missing')
    return flag.ok && flag.value === 'undefined' && sync.ok && sync.value === 2
      ? ok(flag)
      : bad(
          flag.ok && flag.value === 'undefined' ? sync : flag,
          `page script flag ${JSON.stringify(flag.value ?? flag.error)}, evaluate ${JSON.stringify(sync.value ?? sync.error)}`,
        )
  },
)
pageOpsRule(
  'PO.15',
  'run_script and evaluate work on a page with the shell’s CSP (script-src self, no unsafe-eval)',
  'F0: run_script against a page with the shell’s strict CSP',
  (R) => {
    const script = value(R, 'csp.run_script')
    const read = value(R, 'csp.read')
    const sync = value(R, 'csp.sync')
    if (!script || !read || !sync) return none('csp operations missing')
    return script.ok && read.value === 1 && sync.value === 2
      ? ok(read)
      : bad(
          !script.ok ? script : read.value !== 1 ? read : sync,
          'a page operation failed under the CSP',
        )
  },
)
pageOpsRule(
  'PO.16',
  'wait_quiet on an idle page returns after the quiet window and before its timeout',
  'S1m, S1w: quiet detector',
  (R) => {
    const rec = value(R, 'quiet.idle')
    return (
      need(rec, 'quiet.idle') ??
      (rec.ok && rec.ms >= 250 && rec.ms < 6000
        ? ok(rec, `${Math.round(rec.ms)} ms`)
        : bad(rec, rec.ok ? `${Math.round(rec.ms)} ms` : rec.error))
    )
  },
)
const quietAfterResponse = (name, id, note) => (R) => {
  const rec = value(R, name)
  const response = R.recs.find((other) => other.ev === 'scheme.response' && other.id === id)
  if (!rec || !response) return none(`${!rec ? name : `the slow-${id} response`} not recorded`)
  if (!rec.ok) return bad(rec, rec.error)
  return rec.t >= response.t - 5
    ? ok(
        rec,
        `${note}: wait_quiet ended ${Math.round(rec.t - response.t)} ms after the request was answered`,
      )
    : bad(
        rec,
        `${note}: wait_quiet ended ${Math.round(response.t - rec.t)} ms before the request was answered (t ${response.t})`,
      )
}
pageOpsRule(
  'PO.17',
  'wait_quiet does not return while a request that was already in flight when it was called is unanswered',
  'S1m: Resource Timing lists finished requests only; S1w: requests started before the call are not counted',
  quietAfterResponse('quiet.pending', 'b', 'in flight at the call'),
)
pageOpsRule(
  'PO.18',
  'wait_quiet does not return before a request that starts inside the quiet window is answered',
  'S1w: Network events after the call',
  quietAfterResponse('quiet.after', 'c', 'started after the call'),
)
pageOpsRule(
  'PO.19',
  'select all, copy, paste and cut act on the focused textarea (editing commands reach the view)',
  'S1m: pasteAsPlainText:, delete:, selectAll:; S1w: MoveFocus then commands',
  (R) => {
    const selection = value(R, 'input.selection')
    const pasted = value(R, 'input.value.after-paste')
    const cut = value(R, 'input.value.after-cut')
    if (!selection || !pasted || !cut) return none('input operations missing')
    const edits = R.recs.filter(
      (rec) => rec.ev === 'op' && /^input\.edit\.(select-all|copy|paste|cut)/.test(rec.name),
    )
    const failedEdit = edits.find((rec) => !rec.ok)
    if (failedEdit) return bad(failedEdit, failedEdit.error)
    const all =
      selection.value?.[0] === 0 &&
      selection.value?.[1] === selection.value?.[2] &&
      selection.value?.[2] > 0
    return all && pasted.value === 'hello worldhello world' && cut.value === ''
      ? ok(pasted, `selection ${selection.value}, after cut ${JSON.stringify(cut.value)}`)
      : bad(
          !all ? selection : pasted.value !== 'hello worldhello world' ? pasted : cut,
          'an editing command had no effect',
        )
  },
)
pageOpsRule(
  'PO.20',
  'undo restores the cut text',
  'S1m: undo:/redo: fall back to execCommand',
  (R) => {
    const rec = value(R, 'input.value.after-undo')
    return (
      need(rec, 'input.value.after-undo') ??
      info(rec, `value after undo ${JSON.stringify(rec.value)}`)
    )
  },
)
pageOpsRule(
  'PO.21',
  'a key event sent through PageOps::key reaches the page as a trusted keydown and types the character',
  'S1m: a synthesized sendEvent chord is trusted; S7: Space',
  (R) => {
    const typed = value(R, 'input.key.a.read')
    if (!typed) return none('input.key.a.read missing')
    if (!typed.ok) return bad(typed, typed.error)
    const [text, keys] = typed.value
    return String(text).includes('a') && keys?.[0]?.trusted === true
      ? ok(typed)
      : bad(typed, `value ${JSON.stringify(text)} last key ${JSON.stringify(keys)}`)
  },
)
pageOpsRule(
  'PO.22',
  'a Space key sent through PageOps::key types a space into a contenteditable block and counts as user activation',
  'S7: Space gives Blink the user activation the respell needs (P6)',
  (R) => {
    const typed = value(R, 'input.key.space.read')
    if (!typed) return none('input.key.space.read missing')
    if (!typed.ok) return bad(typed, typed.error)
    const [text, activation, keys] = typed.value
    return String(text).length === 4 && keys?.[0]?.trusted === true
      ? ok(typed, `user activation ${activation}`)
      : bad(
          typed,
          `text ${JSON.stringify(text)} activation ${activation} key ${JSON.stringify(keys)}`,
        )
  },
)
pageOpsRule(
  'PO.23',
  'a script added with add_init_script is already set when the next page’s first script runs',
  'S8: initScript before goto; S1m WKUserScript at document start',
  (R) => {
    const rec = firstOf(R, 'po.target.load')
    if (!rec) return none('the target page did not report')
    return rec.initMarker === 42
      ? ok(rec)
      : bad(rec, `initMarker ${JSON.stringify(rec.initMarker)}`)
  },
)

// ------------------------------------------------------------------------------------ P7

const P7 = 'design 0.5 P7'
const P7_VIEW = 'probe-p7-main'
function step(R, name, view = P7_VIEW) {
  const start = R.recs.find((rec) => rec.ev === 'p7.step' && rec.name === name)
  const end = R.recs.find((rec) => rec.ev === 'p7.step.end' && rec.name === name)
  if (!start) return undefined
  const events = R.recs.filter(
    (rec) =>
      rec.src === 'native' &&
      rec.view === (start.view ?? view) &&
      rec._line > start._line &&
      (!end || rec._line < end._line),
  )
  return { start, end, events, kinds: events.map(kindOf) }
}
const stepRule = (id, name, text, source, judge, platforms) =>
  assertion(
    'p7',
    id,
    text,
    source,
    (R) => {
      const found = step(R, name)
      if (!found) return none(`no p7.step ${name} record`)
      return judge(found, R)
    },
    platforms,
  )
stepRule(
  'P7.1',
  'unregistered-scheme',
  'a navigation to a scheme nothing handles fails: NavigationFailed and LoadFinished(ok:false)',
  `${P7}; S1m didFailProvisionalNavigation; S1w NavigationCompleted.IsSuccess`,
  (s) => {
    const failed = s.events.find((rec) => kindOf(rec) === 'NavigationFailed')
    const finished = s.events.find((rec) => kindOf(rec) === 'LoadFinished' && rec.ok === false)
    return failed && finished
      ? ok(failed, `code ${failed.code}`)
      : bad(s.start, `events: ${s.kinds.join(',') || 'none'}`)
  },
)
stepRule(
  'P7.2',
  'app-404',
  'a main document the shell’s own handler answers with 404 is reported as a successful load (so a synthetic NavigationFailed from the handler is needed)',
  `${P7}; browser.rs report_navigation_failure (no caller yet)`,
  (s) => {
    const failed = s.events.find((rec) => kindOf(rec) === 'NavigationFailed')
    const finished = s.events.find((rec) => kindOf(rec) === 'LoadFinished')
    if (failed)
      return bad(
        failed,
        'the platform itself fails a 404 main document; the synthetic report would be redundant',
      )
    return finished?.ok === true
      ? ok(finished, 'LoadFinished ok:true for a 404 page')
      : none(`events: ${s.kinds.join(',') || 'none'}`)
  },
)
stepRule(
  'P7.3',
  'standin-500',
  'a main document answered with 500 is reported as a successful load too',
  P7,
  (s) => {
    const failed = s.events.find((rec) => kindOf(rec) === 'NavigationFailed')
    const finished = s.events.find((rec) => kindOf(rec) === 'LoadFinished')
    if (failed) return bad(failed, 'the platform itself fails a 500 main document')
    return finished?.ok === true ? ok(finished) : none(`events: ${s.kinds.join(',') || 'none'}`)
  },
)
stepRule(
  'P7.4',
  'standin-200',
  'control: the same stand-in scheme with status 200 loads and commits',
  P7,
  (s) => {
    const finished = s.events.find((rec) => kindOf(rec) === 'LoadFinished')
    const committed = s.events.find((rec) => kindOf(rec) === 'DocumentCommitted')
    return finished?.ok === true && committed
      ? ok(finished)
      : bad(s.start, `events: ${s.kinds.join(',') || 'none'}`)
  },
)
stepRule(
  'P7.5',
  'connection-refused',
  'an unreachable http target fails with a platform error code (NavigationFailed, LoadFinished ok:false)',
  `${P7}; S1m, S1w`,
  (s) => {
    const failed = s.events.find((rec) => kindOf(rec) === 'NavigationFailed')
    const finished = s.events.find((rec) => kindOf(rec) === 'LoadFinished' && rec.ok === false)
    return failed && finished
      ? ok(failed, `code ${failed.code}: ${failed.description}`)
      : bad(s.start, `events: ${s.kinds.join(',') || 'none'}`)
  },
)
stepRule(
  'P7.6',
  'supersede',
  'a navigation overtaken by the next one is not reported as a failure (no NavigationFailed; the second load finishes ok)',
  'S1m: -999 ignored; S1w: OperationCanceled not taken for the successor’s failure',
  (s) => {
    const failed = s.events.find((rec) => kindOf(rec) === 'NavigationFailed')
    const finished = s.events.filter((rec) => kindOf(rec) === 'LoadFinished')
    if (failed) return bad(failed, 'the superseded navigation was reported as failed')
    return finished.some((rec) => rec.ok === true)
      ? ok(
          finished.find((rec) => rec.ok === true),
          `events: ${s.kinds.join(',')}`,
        )
      : none(`no LoadFinished ok:true; events: ${s.kinds.join(',') || 'none'}`)
  },
)
stepRule(
  'P7.7',
  'redirect',
  'a redirect keeps one navigation: one NavigationStarted, one DocumentCommitted, no failure',
  'S1w: redirects keep one NavigationId',
  (s) => {
    const started = s.events.filter((rec) => kindOf(rec) === 'NavigationStarted').length
    const committed = s.events.filter((rec) => kindOf(rec) === 'DocumentCommitted').length
    const failed = s.events.find((rec) => kindOf(rec) === 'NavigationFailed')
    if (failed) return bad(failed, 'the redirect failed on this OS')
    return started === 1 && committed === 1
      ? ok(s.start, `events: ${s.kinds.join(',')}`)
      : bad(
          s.start,
          `${started} NavigationStarted, ${committed} DocumentCommitted: ${s.kinds.join(',')}`,
        )
  },
)
stepRule(
  'P7.8',
  'frame',
  'a failing or other-document child frame is not a failure of the page, and its requests carry main_frame=false',
  'S8 inference: PolicyRequest.main_frame; design P7 (main frame only)',
  (s, R) => {
    const failed = s.events.find(
      (rec) =>
        kindOf(rec) === 'NavigationFailed' || (kindOf(rec) === 'LoadFinished' && rec.ok === false),
    )
    if (failed) return bad(failed, 'a child frame failure surfaced as the page’s')
    const frames = R.recs.filter(
      (rec) =>
        rec.ev === 'policy.request' &&
        rec.view === P7_VIEW &&
        rec._line > s.start._line &&
        rec.main_frame === false,
    )
    const mainFrames = R.recs.filter(
      (rec) =>
        rec.ev === 'policy.request' &&
        rec.view === P7_VIEW &&
        rec._line > s.start._line &&
        rec.main_frame === true &&
        /p7-ok\.html/.test(rec.url),
    )
    if (mainFrames.length)
      return bad(mainFrames[0], 'the frame’s document was judged as the main frame')
    return frames.length
      ? ok(frames[0], `${frames.length} frame requests with main_frame=false`)
      : bad(s.start, 'the policy saw no request with main_frame=false for the frames')
  },
)
assertion(
  'p7',
  'P7.9',
  'a main-frame navigation the policy refuses never starts, and the page stays',
  'design 1.11: the navigation hook; S1m observe_navigation_refused',
  (R) => {
    const found = step(R, 'policy-refused', 'probe-p7-refuse')
    if (!found) return none('no p7.step policy-refused record')
    const asked = R.recs.find(
      (rec) =>
        rec.ev === 'policy.request' &&
        rec.view === 'probe-p7-refuse' &&
        /refused-target/.test(rec.url),
    )
    const started = found.events.find(
      (rec) => kindOf(rec) === 'NavigationStarted' && /refused-target/.test(rec.url),
    )
    const stays = !/refused-target/.test(found.end?.url_now ?? '')
    if (!asked) return none('the policy was never asked about the refused URL')
    return !started && stays && asked.allowed === false
      ? ok(asked, `url now ${found.end?.url_now}`)
      : bad(started ?? asked, `started=${!!started} stays=${stays}`)
  },
)
assertion(
  'p7',
  'P7.10',
  'a sub-resource over http to loopback passes a permissive policy',
  'S8 inference: macOS rule list must not block http that the policy allows',
  (R) => {
    const rec = firstOf(R, 'p7.net', (other) => other.tag === 'allow' && other.url !== undefined)
    if (!rec) return none('the allow page did not report')
    return rec.ok && rec.status === 200 ? ok(rec) : bad(rec, rec.error ?? `status ${rec.status}`)
  },
)
assertion(
  'p7',
  'P7.11',
  'a policy that blocks web requests keeps a loopback sub-resource from leaving the page',
  'design 1.11: Blocks on macOS, WebResourceRequested on Windows',
  (R) => {
    const rec = firstOf(R, 'p7.net', (other) => other.tag === 'block' && other.url !== undefined)
    if (!rec) return none('the block page did not report')
    const reached = R.recs.find(
      (other) => other.ev === 'server.request' && /tag=block/.test(other.line),
    )
    if (reached) return bad(reached, 'the request reached the loopback server')
    return !rec.ok ? ok(rec) : bad(rec, 'the page got an answer')
  },
)
assertion(
  'p7',
  'P7.12',
  'a request to a shell scheme that the policy refuses does not get the handler’s answer',
  'S1w: refused scheme requests can be overwritten by wry’s later SetResponse',
  (R) => {
    const rec = firstOf(R, 'p7.refuse')
    if (!rec) return none('the refuse page did not report')
    return !rec.ok || rec.status !== 200
      ? ok(rec)
      : bad(rec, 'the refused request was answered with 200')
  },
  ['windows'],
)
assertion(
  'p7',
  'P7.13',
  'the main-frame navigation reaches the policy with main_frame=true and top_level=true',
  'S8; native/policy.rs flags',
  (R) => {
    const rec = R.recs.find(
      (other) =>
        other.ev === 'policy.request' &&
        other.view === P7_VIEW &&
        /p7-ok\.html/.test(other.url) &&
        other.main_frame === true,
    )
    return rec
      ? rec.top_level === true
        ? ok(rec)
        : bad(rec, 'top_level was false for the main frame')
      : none('no main-frame policy request recorded')
  },
)
assertion(
  'p7',
  'P7.14',
  'a main-frame navigation is judged by the policy exactly once (Windows judged twice before main_frame)',
  'S8 inference: the one-shot guard keeps allowing the document URL because Windows judges twice; F0 follow-up 12',
  (R) => {
    const found = step(R, 'standin-200')
    if (!found) return none('no p7.step standin-200 record')
    const judged = R.recs.filter(
      (rec) =>
        rec.ev === 'policy.request' &&
        rec.view === P7_VIEW &&
        rec._line > found.start._line &&
        (!found.end || rec._line < found.end._line) &&
        rec.main_frame === true &&
        /status\/200/.test(rec.url),
    )
    if (!judged.length) return none('the policy was not asked about the main-frame navigation')
    return judged.length === 1
      ? ok(judged[0])
      : bad(judged[1], `${judged.length} main-frame requests for one navigation`)
  },
)

// ------------------------------------------------------------------------------------ P8

const P8 = 'design 3.6; S7'
assertion('p8', 'P8.1', 'the document bridge is present in the main frame', P8, (R) => {
  const env = firstOf(R, 'p8.env')
  if (!env) return none('no p8.env record')
  return env.bridge === 'function' ? ok(env) : bad(env, 'no __seuofficeCheck in the main frame')
})
assertion(
  'p8',
  'P8.2',
  'the bridge cancels a contextmenu event on text, an editable block, a textarea and an image (it forwards them)',
  `${P8}; invoke-system.js contextmenu forwarder`,
  (R) => {
    const clicks = recsOf(R, 'p8.click')
    if (clicks.length < 4) return none(`${clicks.length} of 4 clicks reported`)
    return combine(
      clicks.map((rec) =>
        item(
          rec.defaultPrevented ? 'observed' : 'violated',
          rec.name,
          rec,
          `defaultPrevented=${rec.defaultPrevented}`,
        ),
      ),
    )
  },
)
assertion(
  'p8',
  'P8.3',
  'the forwarded request reached the shell’s real ingress and handler (the handler logged its refusal of the probe’s view)',
  'S7: ipc_context_menu → native::menu::install handler',
  (R) => {
    if (R.log === undefined) return none('no shell.log beside the record')
    const lines = R.log
      .split('\n')
      .filter((text) => /a context menu of 'probe-p8-main' was refused/.test(text))
    return lines.length >= 2
      ? ok(undefined, `shell.log: ${lines.length} refusals, e.g. ${lines[0].trim()}`)
      : lines.length
        ? ok(undefined, `shell.log: ${lines[0].trim()} (only ${lines.length})`)
        : bad(undefined, 'no refusal line in shell.log')
  },
)
assertion(
  'p8',
  'P8.4',
  'the command accepts the bridge’s payload with the nonce',
  'S7: Payload::parse; nonce ingress',
  (R) => {
    const rec = firstOf(R, 'p8.invoke')
    if (!rec) return none('no p8.invoke record')
    return rec.ok ? ok(rec) : bad(rec, rec.error)
  },
)
assertion(
  'p8',
  'P8.5',
  'a right click inside the child frame is not cancelled and the child has no bridge (the OS default menu shows)',
  'design 3.6: child frames do not run the main-frame bridge',
  (R) => {
    const rec = firstOf(R, 'p8.child')
    if (!rec?.answer)
      return none(rec ? `the child call failed: ${rec.error}` : 'no p8.child record')
    return rec.answer.defaultPrevented === false && rec.answer.bridge === 'undefined'
      ? ok(rec)
      : bad(rec, 'the bridge ran in the child frame')
  },
)
assertion(
  'p8',
  'P8.6',
  'the main page reads the child frame’s location and calls its window (same origin)',
  'design 3.5, 3.6',
  (R) => {
    const rec = firstOf(R, 'p8.child')
    if (!rec) return none('no p8.child record')
    return String(rec.href).includes('/studio/') && rec.answer
      ? ok(rec)
      : bad(rec, rec.error ?? 'the child frame was not reachable')
  },
)
assertion(
  'p8',
  'P8.7',
  'caretRangeFromPoint finds the text node under a point and execCommand(insertText) edits a contenteditable block and a textarea',
  'S7: replace_misspelling_script',
  (R) => {
    const caret = firstOf(R, 'p8.caret')
    const area = firstOf(R, 'p8.area')
    if (!caret || !area) return none('the caret records are missing')
    const good =
      caret.rangeNode === 3 &&
      caret.insertText === true &&
      String(caret.after).startsWith('the') &&
      area.insert === true &&
      String(area.value).startsWith('TEXT')
    return good
      ? ok(caret, `after ${JSON.stringify(caret.after)}, textarea ${JSON.stringify(area.value)}`)
      : bad(
          caret,
          `rangeNode ${caret.rangeNode}, insertText ${caret.insertText}, after ${JSON.stringify(caret.after)}; textarea ${JSON.stringify(area.value)}`,
        )
  },
)

// ------------------------------------------------------------------------------------ P9

const P9 = 'design 0.5 P9; S3'
const documentAt = (R, view, rec) => {
  const registered = nativeOf(R, view).filter(
    (other) => kindOf(other) === 'DocumentRegistered' && other._line < rec._line,
  )
  return registered.at(-1)?.document
}
for (const [id, variant, note] of [
  ['P9.1', 'open', 'window.open(url)'],
  ['P9.2', 'features', 'window.open(url, name, features)'],
  ['P9.3', 'anchor', 'a script-clicked <a target=_blank>'],
  ['P9.4', 'iframe', 'window.open called from a child frame'],
  ['P9.5', 'key', 'window.open inside a trusted keydown (user activation)'],
]) {
  assertion('p9', id, `${note} reaches on_new_window with the absolute url`, P9, (R) => {
    const call = R.recs.find(
      (rec) =>
        rec.ev === 'win.new_window' && String(rec.url).includes(`p9-target.html?v=${variant}`),
    )
    if (call) return ok(call)
    const asked = R.recs.find((rec) => rec.ev === 'p9.variant' && rec.name === variant)
    return asked
      ? bad(asked, 'the page asked for the popup and on_new_window never fired')
      : none(`the page never asked for the ${variant} popup`)
  })
}
assertion('p9', 'P9.6', 'on_new_window runs on the UI thread', P9, (R) => {
  const calls = recsOf(R, 'win.new_window')
  if (!calls.length) return none('on_new_window never fired')
  const off = calls.find((rec) => rec.ui_thread !== true)
  return off ? bad(off) : ok(calls[0], `${calls.length} calls`)
})
assertion(
  'p9',
  'P9.7',
  'the popup is attributed to the registered document of the browser that opened it (Native::current_context inside the callback)',
  'W2; S3 popups use native.current_context(label)',
  (R) => {
    const calls = recsOf(R, 'win.new_window')
    if (!calls.length) return none('on_new_window never fired')
    return combine(
      calls.map((rec) => {
        const expected = documentAt(R, rec.view, rec)
        const label = String(rec.url).split('?v=')[1]
        return item(
          rec.registered_document !== null && rec.registered_document === expected
            ? 'observed'
            : 'violated',
          label,
          rec,
          `callback saw document ${rec.registered_document}, last registered ${expected}`,
        )
      }),
    )
  },
)
assertion(
  'p9',
  'P9.8',
  'navigate_observed(about:blank) on a registered document completes with the blank page',
  'S4: release_owned; design W3',
  (R) => {
    const rec = firstOf(R, 'p9.observed')
    if (!rec) return none('no p9.observed record')
    return rec.dispatched && !rec.timeout && rec.answer === 'about:blank'
      ? ok(rec)
      : bad(
          rec,
          rec.dispatch_error ??
            rec.answer_error ??
            (rec.timeout ? 'no commit within 15 s' : 'wrong answer'),
        )
  },
)
assertion(
  'p9',
  'P9.9',
  'request_close (Webview::close) succeeds and the label is gone from Tauri’s registry when it returns',
  'S3, design 1.9: Retired = close dispatched and the label gone',
  (R) => {
    const rec = firstOf(R, 'p9.close')
    if (!rec) return none('no p9.close record')
    return rec.request_ok && rec.gone_at_return
      ? ok(rec)
      : bad(
          rec,
          rec.request_error ??
            `removed ${rec.removed_after_ms === null ? 'never' : `after ${Math.round(rec.removed_after_ms)} ms`}`,
        )
  },
)
assertion('p9', 'P9.10', 'the Retired event follows a close', 'design 1.9', (R) => {
  const rec = firstOf(R, 'p9.close')
  if (!rec) return none('no p9.close record')
  return rec.retired_event ? ok(rec) : bad(rec, 'no Retired event within 5 s')
})
assertion(
  'p9',
  'P9.11',
  'destroying a window retires its browser (Native::on_window_event forwarded)',
  'S3: main.rs must forward on_window_event; S6',
  (R) => {
    const rec = firstOf(R, 'p9.window_retired')
    if (!rec) return none('no p9.window_retired record')
    return rec.retired
      ? ok(rec, `${Math.round(rec.ms)} ms`)
      : bad(rec, 'no Retired event within 8 s')
  },
)
assertion(
  'p9',
  'P9.12',
  'each of two windows gets a View(Focus) event when it is given the focus',
  'S7: focus events per view feed FocusCapture',
  (R) => {
    const focus = recsOf(R, 'p9.focus')
    if (focus.length < 2) return none(`${focus.length} of 2 focus records`)
    return combine(
      focus.map((rec) =>
        item(
          rec.focus_event ? 'observed' : 'violated',
          rec.view,
          rec,
          `set_focus ${rec.set_focus_ok ? 'ok' : rec.set_focus_error}, is_focused ${rec.is_focused}`,
        ),
      ),
    )
  },
)
assertion(
  'p9',
  'P9.13',
  'the shell’s own Windows opens a window and a transparent child window parented to it',
  'S3: window_created vs attach reservation; .parent() and .transparent() with the .native hooks',
  (R) => {
    const real = recsOf(R, 'p9.real')
    if (real.length < 2) return none(`${real.length} of 2 real window records`)
    return combine(
      real.map((rec) => item(rec.ok ? 'observed' : 'violated', rec.what, rec, rec.error)),
    )
  },
)
assertion(
  'p9',
  'P9.14',
  'the bridge does not register the about:blank document that release_owned navigates to',
  'S4 inference: a registration on the blank page would be ignored by the frozen entry',
  (R) => {
    const observed = firstOf(R, 'p9.observed')
    if (!observed) return none('no p9.observed record')
    const registered = nativeOf(R, 'probe-p9-child').find(
      (rec) => kindOf(rec) === 'DocumentRegistered' && String(rec.url).startsWith('about:'),
    )
    return registered
      ? bad(registered, 'the blank document was registered')
      : ok(observed, 'no DocumentRegistered for about:blank')
  },
)

// ----------------------------------------------------------------------------------- P10

const REFERENCE = { macos: 64.6, windows: 55.8 }
const P10 = 'README.md measurement; decision 0029; design 0.5 P10'
const dialog = (R) => R.report
assertion('p10', 'P10.1', 'the measurement ran to its report without an error', P10, (R) => {
  const report = dialog(R)
  if (!report) return none(R.note ?? 'no dialog.json')
  return report.error
    ? bad(undefined, report.error)
    : report.mode === 'measure-dialog'
      ? ok(undefined, `os ${report.environment?.os}, ${report.environment?.profile}`)
      : bad(undefined, 'not a measure-dialog report')
})
assertion(
  'p10',
  'P10.2',
  '1 KB and 10 MB replies come back intact through the outbox transport',
  `${P10}; design 1.4`,
  (R) => {
    const echo = dialog(R)?.echo
    if (!echo) return none('no echo section')
    const bad2 = ['1KB', '10MB'].find((name) => !echo[name]?.intact)
    return bad2
      ? bad(undefined, `${bad2} was not intact: ${JSON.stringify(echo[bad2])}`)
      : ok(
          undefined,
          `1KB p50 ${echo['1KB'].p50?.toFixed(1)} ms, 10MB p50 ${echo['10MB'].p50?.toFixed(1)} ms`,
        )
  },
)
assertion(
  'p10',
  'P10.3',
  `the 10 MB echo p50 is within ${P10_RATIO}x of the prototype value (macOS 64.6 ms, Windows 55.8 ms)`,
  `${P10}; the ratio is this checker’s choice (--p10-ratio), the outbox adds a hop the prototype did not have`,
  (R) => {
    const report = dialog(R)
    const reference = REFERENCE[R.platform]
    if (!report?.echo?.['10MB'] || !reference) return none('no 10MB echo or unknown OS')
    const p50 = report.echo['10MB'].p50
    const ratio = p50 / reference
    return ratio <= P10_RATIO
      ? ok(undefined, `p50 ${p50.toFixed(1)} ms = ${ratio.toFixed(2)}x of ${reference} ms`)
      : bad(undefined, `p50 ${p50.toFixed(1)} ms = ${ratio.toFixed(2)}x of ${reference} ms`)
  },
)
assertion(
  'p10',
  'P10.4',
  '100 synchronous XHR calls (sendSync) succeed',
  'P1; README measurement 4',
  (R) => {
    const sync = dialog(R)?.sync
    if (!sync) return none('no sync section')
    return !sync.error && sync.n === 100
      ? ok(undefined, `p50 ${sync.p50?.toFixed(1)} ms`)
      : bad(undefined, sync.error ?? `n ${sync.n}`)
  },
)
assertion(
  'p10',
  'P10.5',
  'every call held behind a blocked core thread is answered once it ends (no call left unanswered)',
  'contract 3: the known difference',
  (R) => {
    const report = dialog(R)?.dialog
    if (!report) return none('no dialog section')
    const lost = ['ui', 'core'].filter((kind) => report[kind]?.unanswered !== 0)
    return lost.length
      ? bad(
          undefined,
          `unanswered calls in ${lost.join(', ')}: ${JSON.stringify(lost.map((kind) => report[kind]?.unanswered))}`,
        )
      : ok(
          undefined,
          `during p95 ui ${report.ui.during?.p95?.toFixed(0)} ms, core ${report.core.during?.p95?.toFixed(0)} ms`,
        )
  },
)
assertion('p10', 'P10.6', 'idle round trip', P10, (R) => {
  const idle = dialog(R)?.idle
  return idle
    ? info(
        undefined,
        `p50 ${idle.p50?.toFixed(2)} ms, p95 ${idle.p95?.toFixed(2)} ms, max ${idle.max?.toFixed(2)} ms`,
      )
    : none('no idle section')
})

// ----------------------------------------------------------------------------------- running

const results = []
for (const probe of wanted) {
  const R = load(probe)
  R.recs.forEach((rec) => {
    rec._probe = `${probe}.jsonl:`
  })
  const mine = rows.filter((row) => row.probe === probe)
  if (R.skipped) {
    for (const row of mine)
      results.push({
        ...row,
        status: 'n/a',
        evidence: `the probe skipped itself on ${R.skipped.platform}: it is a ${R.skipped.wanted} probe`,
      })
    continue
  }
  if (!mine.length) continue
  if (R.missing) {
    for (const row of mine)
      results.push({
        ...row,
        status: 'unobserved',
        evidence: R.note ?? `no record in ${R.dir}: the probe did not run or wrote nothing`,
      })
    continue
  }
  if (probe !== 'p10') {
    const ended = R.ended
    const status = ended?.ok ? 'observed' : 'unobserved'
    results.push({
      probe,
      id: `${probe.toUpperCase()}.run`,
      text: 'the probe ran to its end',
      source: 'harness',
      status,
      evidence: ended
        ? at(ended)
        : `no probe.end line (${R.recs.at(-1) ? at(R.recs.at(-1)) : 'empty record'}); ${recsOf(R, 'probe.timeout').length ? 'it hit its deadline' : 'the process may have crashed'}`,
    })
  }
  for (const row of mine) {
    if (row.platforms && R.platform && !row.platforms.includes(R.platform)) {
      results.push({
        ...row,
        status: 'n/a',
        evidence: `an assertion about ${row.platforms.join('/')}; this run is ${R.platform}`,
      })
      continue
    }
    let verdict
    try {
      verdict = row.check(R)
    } catch (error) {
      verdict = {
        status: 'unobserved',
        evidence: `the checker failed on this record: ${error.stack?.split('\n')[0] ?? error}`,
      }
    }
    results.push({ ...row, ...verdict })
  }
}

const mark = {
  observed: 'OBSERVED  ',
  violated: 'VIOLATED  ',
  unobserved: 'UNOBSERVED',
  info: 'info      ',
  'n/a': 'n/a       ',
}
const lines = []
for (const row of results) {
  lines.push(`${mark[row.status]} ${row.id.padEnd(7)} ${row.text}`)
  lines.push(`           source: ${row.source}`)
  if (row.evidence) lines.push(`           evidence: ${row.evidence}`)
}
const count = (status) => results.filter((row) => row.status === status).length
const summary = `${count('observed')} observed, ${count('violated')} violated, ${count('unobserved')} unobserved, ${count('info')} info, ${count('n/a')} n/a`
console.log(lines.join('\n'))
console.log(`\n${summary}`)

const jsonOut = option('--json')
if (jsonOut)
  fs.writeFileSync(
    jsonOut,
    JSON.stringify(
      results.map(({ check: _check, ...rest }) => rest),
      null,
      2,
    ),
  )
const markdownOut = option('--markdown')
if (markdownOut) {
  const table = ['| status | id | assertion | source | evidence |', '|---|---|---|---|---|']
  const cell = (text) =>
    String(text ?? '')
      .replace(/\|/g, '\\|')
      .replace(/\n/g, ' ')
  for (const row of results)
    table.push(
      `| ${row.status} | ${row.id} | ${cell(row.text)} | ${cell(row.source)} | ${cell(row.evidence)} |`,
    )
  fs.writeFileSync(markdownOut, `## WebView probes\n\n${summary}\n\n${table.join('\n')}\n`)
}
process.exit(count('violated') ? 1 : count('unobserved') ? 2 : 0)
