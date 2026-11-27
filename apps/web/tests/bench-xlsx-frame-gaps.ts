/** Full rAF intervals overlapping the wheel window, including stalls across either edge. */
export function frameGaps(frames: readonly number[], from: number, to: number) {
  if (!frames.length || frames[0]! > from || frames[frames.length - 1]! < to) {
    throw new Error('rAF samples do not cover the measurement window')
  }
  const gaps: { atMs: number; gapMs: number }[] = []
  for (let i = 1; i < frames.length; i++) {
    const end = frames[i]!
    const start = frames[i - 1]!
    if (start >= to) break
    if (end > from) gaps.push({ atMs: end - from, gapMs: end - start })
  }
  return gaps
}
