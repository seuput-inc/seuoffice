/** The reports of a run as a table: what each frame measured as shown, and with autosizing off */

const px = (n) => `${Math.round(n * 100) / 100}px`

function sizes(groups, region) {
  const rows = (groups ?? []).filter((g) => g.region === region)
  if (!rows.length) return '(no text)'
  return rows
    .slice(0, 4)
    .map((g) => `${px(g.fontSize)} ×${g.chars}ch (line-height ${g.lineHeight}, block ${g.blockWidth}px)`)
    .join('<br>')
}

function changes(changed, region) {
  const rows = (changed ?? []).filter((c) => c.region === region)
  if (!rows.length) return 'none'
  return rows
    .slice(0, 4)
    .map((c) => `${px(c.from)} → ${px(c.to)} ×${c.chars}ch`)
    .join('<br>')
}

export function summarize(reports, cases, device) {
  const lines = [
    `Device: ${device}`,
    '',
    '| case | frame | opened | viewport | dpr | html text-size-adjust | document text as shown | document text with 100% | ui text as shown | sizes that changed with 100% (doc / ui) |',
    '| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |',
  ]
  for (const test of cases) {
    const mine = reports.filter((r) => r.case === test.name)
    const host = mine.find((r) => r.frame === 'host')
    const opened = host ? (host.event === 'opened' ? 'yes' : `no: ${host.message}`) : 'no report'
    const frames = [...new Set(mine.filter((r) => r.frame !== 'host').map((r) => r.frame))]
    if (!frames.length) lines.push(`| ${test.name} | (none reported) | ${opened} | | | | | | | |`)
    for (const frame of frames) {
      const shown = mine.find((r) => r.frame === frame && r.phase === 'measure')
      const forced = mine.find((r) => r.frame === frame && r.phase === 'force')
      const env = (shown ?? forced)?.env
      lines.push(
        `| ${test.name} | ${frame} | ${opened} | ${env ? `${env.visualViewport?.width ?? '?'} (inner ${env.inner[0]}, host ${host?.env?.inner?.[0] ?? '?'})` : ''} | ${env?.dpr ?? ''} | ${env?.htmlTextSizeAdjust ?? ''} | ${sizes(shown?.groups, 'doc')} | ${sizes(forced?.groups, 'doc')} | ${sizes(shown?.groups, 'ui')} | ${changes(forced?.changed, 'doc')} / ${changes(forced?.changed, 'ui')} |`,
      )
    }
  }
  return lines.join('\n') + '\n'
}
