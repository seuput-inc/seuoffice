import { compact, profile } from './pdf-envelope'

type Value = string | Value[] | Dict
type Dict = Map<string, Value>

// The profile uses only direct, shallow dictionaries and arrays. Keep refs as single tokens;
// only the field's declared role may follow one into the document graph.
export function dictionary(source: string): Dict {
  profile(source.length <= 32768)
  const tokens = compact(source).split(' ')
  profile(tokens.length <= 1024)
  let i = 0
  function item(depth: number): Value {
    profile(depth <= 6)
    const t = tokens[i++]
    if (t === '<<') {
      const result: Dict = new Map()
      while (tokens[i] !== '>>') {
        const key = tokens[i++]
        profile(key && /^\/[A-Za-z0-9_-]{1,80}$/.test(key) && !result.has(key) && result.size < 32)
        result.set(key, item(depth + 1))
      }
      i++
      return result
    }
    if (t === '[') {
      const result: Value[] = []
      while (tokens[i] !== ']') {
        profile(result.length < 256)
        result.push(item(depth + 1))
      }
      i++
      return result
    }
    if (t && /^[1-9]\d{0,2}$/.test(t) && tokens[i] === '0' && tokens[i + 1] === 'R') {
      i += 2
      return `${t} 0 R`
    }
    profile(
      t &&
        /^(?:\/[A-Za-z0-9_-]{1,80}|-?\d{1,5}(?:\.\d{1,24})?|<(?:[\dA-Fa-f]{2}){0,2048}>)$/.test(t),
    )
    return t
  }
  const result = item(0)
  profile(i === tokens.length && result instanceof Map)
  return result
}

export function dict(value: Value | undefined, ...allowed: string[]): Dict {
  profile(value instanceof Map)
  for (const key of value.keys()) profile(allowed.includes(key))
  return value
}

export function array(value: Value | undefined, min: number, max: number): Value[] {
  profile(Array.isArray(value) && value.length >= min && value.length <= max)
  return value
}

export function num(value: Value | undefined, min: number, max: number): number {
  profile(typeof value === 'string' && /^-?\d{1,5}(?:\.\d{1,24})?$/.test(value))
  const n = Number(value)
  profile(n >= min && n <= max)
  return n
}

export function ref(value: Value | undefined): number {
  profile(typeof value === 'string' && /^[1-9]\d{0,2} 0 R$/.test(value))
  return parseInt(value, 10)
}
