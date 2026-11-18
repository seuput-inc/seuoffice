// The DevTools protocol, the part the migration test needs: find the page
// that exposes a bridge global and evaluate in it. Electron and the CEF shell
// both accept --remote-debugging-port, and both expose the same preload
// globals (window.aiOffice, window.aiOfficeUpdate), so one checker reads the
// old and the new app alike. Node 22+ (global fetch and WebSocket).

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

/**
 * The pages of the app, the shell's own first: a document tab (mode=tab) has the same
 * bridge globals, and which page a read lands on must not depend on whether a tab is open.
 */
async function pages(port) {
  const res = await fetch(`http://127.0.0.1:${port}/json/list`)
  const list = (await res.json()).filter((t) => t.type === 'page' && t.webSocketDebuggerUrl)
  return list.sort(
    (a, b) => Number(a.url.includes('mode=tab')) - Number(b.url.includes('mode=tab')),
  )
}

async function connect(target) {
  const ws = new WebSocket(target.webSocketDebuggerUrl)
  await new Promise((resolve, reject) => {
    ws.onopen = resolve
    ws.onerror = () => reject(new Error(`cannot connect to ${target.url}`))
  })
  let last = 0
  const waiting = new Map()
  ws.onmessage = (event) => {
    const msg = JSON.parse(event.data)
    waiting.get(msg.id)?.resolve(msg)
    waiting.delete(msg.id)
  }
  ws.onclose = () => {
    for (const waiter of waiting.values()) waiter.reject(new Error(`${target.url} closed`))
    waiting.clear()
  }
  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = ++last
      waiting.set(id, { resolve, reject })
      ws.send(JSON.stringify({ id, method, params }))
    })
  const evaluate = async (expression) => {
    const reply = await send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    })
    if (reply.error) throw new Error(reply.error.message)
    const { result, exceptionDetails } = reply.result
    if (exceptionDetails) {
      throw new Error(exceptionDetails.exception?.description ?? exceptionDetails.text)
    }
    return result.value
  }
  return { send, evaluate, close: () => ws.close() }
}

async function evaluateIn(target, expression) {
  const session = await connect(target)
  try {
    return await session.evaluate(expression)
  } finally {
    session.close()
  }
}

/** Waits for a page where `global` is an object, then evaluates `expression` there */
export async function evaluate(port, global, expression, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs
  let last
  while (Date.now() < deadline) {
    try {
      for (const target of await pages(port)) {
        if (await evaluateIn(target, `typeof window.${global} === 'object'`)) {
          return await evaluateIn(target, expression)
        }
      }
    } catch (err) {
      last = err
    }
    await sleep(1000)
  }
  const why = last
    ? `${last.message}${last.cause ? ` (${last.cause.code ?? last.cause.message})` : ''}`
    : ''
  throw new Error(`no page with window.${global} on port ${port}${why ? `: ${why}` : ''}`)
}

/**
 * An open connection to the first page whose URL contains `urlPart`, for what needs more than
 * Runtime.evaluate (input events): { send(method, params), evaluate(expression), close() }
 */
export async function pageSession(port, urlPart, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs
  let last
  while (Date.now() < deadline) {
    try {
      const target = (await pages(port)).find((t) => t.url.includes(urlPart))
      if (target) return await connect(target)
    } catch (err) {
      last = err
    }
    await sleep(1000)
  }
  throw new Error(`no page with ${urlPart} on port ${port}${last ? `: ${last.message}` : ''}`)
}
