/** The reports of a run as a table: what each frame measured as shown and under each variant */

const px = (n) => `${Math.round(n * 100) / 100}px`

function sizes(report, region) {
  const rows = (report?.groups ?? []).filter((g) => g.region === region)
  if (!rows.length) return '(no text)'
  const zoom = region === 'doc' ? (report.env.docZoom ?? 1) : 1
  return rows
    .slice(0, 3)
    .map(
      (g) =>
        `${px(g.fontSize)}${zoom === 1 ? '' : ` (drawn ${px(g.fontSize * zoom)})`} ×${g.chars}ch, ` +
        `line-height ${Number.isNaN(parseFloat(g.lineHeight)) ? g.lineHeight : px(parseFloat(g.lineHeight))}, first block ${g.lines} lines in ${g.blockWidth}px` +
        (g.adjust && g.adjust !== report.env.htmlTextSizeAdjust ? `, adjust ${g.adjust}` : ''),
    )
    .join('<br>')
}

export function summarize(reports, cases, device) {
  const lines = [
    `Device: ${device}`,
    '',
    '| case | frame | variant | viewport | dpr | html text-size-adjust | zoom | pages | document text | ui text |',
    '| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |',
  ]
  for (const test of cases) {
    const mine = reports.filter((r) => r.case === test.name)
    const host = mine.find((r) => r.frame === 'host')
    const opened = host ? (host.event === 'opened' ? '' : ` (not opened: ${host.message})`) : ' (host silent)'
    const frames = [...new Set(mine.filter((r) => r.frame !== 'host').map((r) => r.frame))]
    if (!frames.length) lines.push(`| ${test.name}${opened} | (none reported) | | | | | | | | |`)
    for (const frame of frames)
      for (const report of mine.filter((r) => r.frame === frame)) {
        const env = report.env
        lines.push(
          `| ${test.name}${opened} | ${frame} | ${report.phase} | ${env.visualViewport?.width ?? '?'} (inner ${env.inner[0]}, host ${host?.env?.inner?.[0] ?? '?'}) | ${env.dpr} | ${env.htmlTextSizeAdjust ?? 'unknown to the engine'} | ${Math.round(env.docZoom * 1000) / 1000} | ${env.pages ?? ''} | ${sizes(report, 'doc')} | ${sizes(report, 'ui')} |`,
        )
      }
  }
  return lines.join('\n') + '\n'
}
