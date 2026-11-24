import { isStudioReadCall } from './read-only.mjs'

/** Only local Studio calls cross this facade. Replay uses the owner's raw document. */
export function wrapHwpMutations(doc, onMutation) {
  const methods = Object.getPrototypeOf(doc)
  const wrapped = new Map()
  return new Proxy(doc, {
    get(target, key, receiver) {
      const value = Reflect.get(target, key, receiver)
      if (typeof key !== 'string' || typeof value !== 'function' || !Object.hasOwn(methods, key))
        return value
      if (isStudioReadCall(key)) return value
      if (!wrapped.has(key)) {
        wrapped.set(key, (...args) =>
          onMutation(target, key, args, (...replacement) =>
            Reflect.apply(value, target, replacement.length ? replacement : args),
          ),
        )
      }
      return wrapped.get(key)
    },
  })
}
