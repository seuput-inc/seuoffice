import { HistoryError, type HistoryRequest } from '@seuoffice/collab/client'

function invalid(): never {
  throw new HistoryError('invalid_history', 'expected a bounded JSON history prefix')
}

export function checkJsonBudget(value: unknown): void {
  let nodes = 16_384
  let text = 256 * 1024
  function charge(s: string) {
    if (s.length > text || (text -= Buffer.byteLength(s)) < 0) invalid()
  }
  function visit(value: unknown, depth: number): void {
    if (--nodes < 0 || depth > 32) invalid()
    if (typeof value === 'string') return charge(value)
    if (value === null || typeof value === 'boolean' || Number.isFinite(value)) return
    if (typeof value !== 'object') invalid()
    if (Array.isArray(value)) {
      if (value.length > nodes) invalid()
      for (const field of value) visit(field, depth + 1)
    } else {
      for (const [key, field] of Object.entries(value)) {
        if (key === '__proto__' || key === 'prototype' || key === 'constructor') invalid()
        charge(key)
        visit(field, depth + 1)
      }
    }
  }
  visit(value, 0)
}

export function checkBudget(request: HistoryRequest): void {
  checkJsonBudget(request)
  if (!Array.isArray(request?.changes) || request.changes.length > 128) invalid()
  let ops = 0
  for (const change of request.changes) {
    if (!Array.isArray(change?.ops) || (ops += change.ops.length) > 256) invalid()
  }
}
