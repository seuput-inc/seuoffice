/** References an argument of an earlier renderer event on the same side. */
import { isDeepStrictEqual } from 'node:util'
import type { Recorded } from './fakes.ts'
import type { Call } from './types.ts'

export const VOID_ANSWER = { value: { $: 'u' }, buffers: [] }
export const rejectedAnswer = (message: string): { error: string } => ({
  error: `Error: ${message}`,
})

export interface EventArgument {
  step: number
  channel: string
  view: number
  arg: number
  occurrence: number
}

export const eventArg = (
  step: number,
  channel: string,
  view = 1,
  arg = 0,
  occurrence = 0,
): unknown => ({
  $eventArg: { step, channel, view, arg, occurrence },
})

function reference(value: unknown): EventArgument | undefined {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return undefined
  const object = value as Record<string, unknown>
  if (Object.keys(object).length !== 1 || !Object.hasOwn(object, '$eventArg')) return undefined
  const ref = object.$eventArg
  if (typeof ref !== 'object' || ref === null || Array.isArray(ref))
    throw new Error('invalid event argument reference')
  const r = ref as Record<string, unknown>
  if (
    !Number.isSafeInteger(r.step) ||
    !Number.isSafeInteger(r.view) ||
    !Number.isSafeInteger(r.arg) ||
    !Number.isSafeInteger(r.occurrence) ||
    typeof r.channel !== 'string' ||
    (r.step as number) < 0 ||
    (r.view as number) < 0 ||
    (r.arg as number) < 0 ||
    (r.occurrence as number) < 0
  ) {
    throw new Error('invalid event argument reference')
  }
  return {
    step: r.step as number,
    channel: r.channel,
    view: r.view as number,
    arg: r.arg as number,
    occurrence: r.occurrence as number,
  }
}

const key = (r: EventArgument): string =>
  JSON.stringify([r.step, r.channel, r.view, r.arg, r.occurrence])

/** Captures actual wire values, while normalizing only those identities for comparison. */
export class EventReferences {
  private readonly wanted = new Map<string, EventArgument>()
  private readonly values = new Map<string, unknown>()

  constructor(calls: readonly Call[]) {
    const visit = (value: unknown): void => {
      const ref = reference(value)
      if (ref) {
        this.wanted.set(key(ref), ref)
        return
      }
      if (Array.isArray(value)) for (const item of value) visit(item)
      else if (typeof value === 'object' && value !== null)
        for (const item of Object.values(value)) visit(item)
    }
    for (const call of calls) {
      visit(call.args)
      visit(call.capture)
    }
  }

  resolve(value: unknown): unknown {
    const ref = reference(value)
    if (ref) {
      const id = key(ref)
      if (!this.values.has(id)) throw new Error(`event argument not captured: ${id}`)
      return this.values.get(id)
    }
    if (Array.isArray(value)) return value.map((item) => this.resolve(item))
    if (typeof value === 'object' && value !== null) {
      return Object.fromEntries(
        Object.entries(value).map(([name, item]) => [name, this.resolve(item)]),
      )
    }
    return value
  }

  capture(step: number, events: Recorded[]): Recorded[] {
    const output = events.map((event) => ({
      ...event,
      args: Array.isArray(event.args) ? [...event.args] : event.args,
    }))
    for (const [id, ref] of this.wanted) {
      if (ref.step !== step) continue
      const candidates = events.filter(
        (event) => event.channel === ref.channel && event.view === ref.view,
      )
      const event = candidates[ref.occurrence]
      if (!event || !Array.isArray(event.args) || ref.arg >= event.args.length) {
        throw new Error(`event argument not found: ${id}`)
      }
      const value = event.args[ref.arg]
      if (typeof value !== 'number' && typeof value !== 'string')
        throw new Error(`event identity is not a number or string: ${id}`)
      this.values.set(id, value)
      const index = events.indexOf(event)
      const args = output[index]?.args
      if (!Array.isArray(args)) throw new Error(`event arguments are not an array: ${id}`)
      args[ref.arg] = `$EVENT:${id}:${typeof value}`
    }
    return output
  }
}

/** Both sides must satisfy explicitly specified outcomes, even if they agree. */
export function outcomeFailure(
  calls: readonly Call[],
  answers: readonly { answer: unknown }[],
): string | undefined {
  for (const [index, call] of calls.entries()) {
    if (
      call.expectAnswer !== undefined &&
      !isDeepStrictEqual(answers[index]?.answer, call.expectAnswer)
    ) {
      return `step ${index}: expected ${JSON.stringify(call.expectAnswer)}, got ${JSON.stringify(answers[index]?.answer)}`
    }
  }
  return undefined
}
