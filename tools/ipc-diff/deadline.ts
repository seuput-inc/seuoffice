/**
 * A bound on one side of a case (main.ts). Work that never settles (a promise
 * nobody resolves, an event that never comes) holds the whole step when
 * nothing bounds it, so it fails its case instead.
 */

/** Work that was still pending when its bound ran out */
export class Hang extends Error {}

/**
 * Marks the timer of a bound: a case's fake clock (d1/mindmap-host.ts) lets the
 * case's own time pass, never the bound's
 */
export const HARNESS_TIMER = Symbol.for('ipc-diff.harness-timer')

/**
 * `work`, or a rejection with a `Hang` saying `what` did not finish in `ms`
 * milliseconds. The timer is not unreferenced: it fires even when nothing
 * else is scheduled.
 */
export function withDeadline<T>(what: string, ms: number, work: Promise<T>): Promise<T> {
  let timer: NodeJS.Timeout | undefined
  const late = new Promise<never>((_, reject) => {
    const fire = (): void => reject(new Hang(`${what} did not finish in ${ms / 1000} s`))
    Object.defineProperty(fire, HARNESS_TIMER, { value: true })
    timer = setTimeout(fire, ms)
  })
  return Promise.race([work, late]).finally(() => clearTimeout(timer))
}
