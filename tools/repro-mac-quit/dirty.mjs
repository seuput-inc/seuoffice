// Best effort: type into the open Word tab through the DevTools protocol so the
// document has unsaved changes, then print what the page says. Whether the
// document really became dirty shows later, when the quit trace sees (or does not
// see) the unsaved-changes dialog.
//
//   node dirty.mjs <port>
const port = process.argv[2]
const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()
const page = list.find((t) => t.type === 'page' && t.url.includes('docs/index.html'))
if (!page) {
  console.log('no docs page')
  process.exit(1)
}
const ws = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((resolve, reject) => {
  ws.onopen = resolve
  ws.onerror = () => reject(new Error('cannot connect'))
})
let id = 0
const pending = new Map()
ws.onmessage = (event) => {
  const msg = JSON.parse(event.data)
  pending.get(msg.id)?.(msg)
}
const send = (method, params = {}) =>
  new Promise((resolve) => {
    const n = ++id
    pending.set(n, resolve)
    ws.send(JSON.stringify({ id: n, method, params }))
  })
const info = await send('Runtime.evaluate', {
  expression: `JSON.stringify({ w: innerWidth, h: innerHeight, active: document.activeElement?.tagName, title: document.title })`,
  returnByValue: true,
})
console.log('page', info.result?.result?.value)
for (const [x, y] of [
  [500, 300],
  [400, 400],
]) {
  for (const type of ['mousePressed', 'mouseReleased']) {
    await send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1 })
  }
}
await send('Input.insertText', { text: 'unsaved change ' })
for (const key of ['a', 'b', 'c']) {
  await send('Input.dispatchKeyEvent', { type: 'keyDown', key, text: key })
  await send('Input.dispatchKeyEvent', { type: 'keyUp', key })
}
const after = await send('Runtime.evaluate', {
  expression: `JSON.stringify({ active: document.activeElement?.tagName, text: (document.body.innerText || '').slice(0, 200) })`,
  returnByValue: true,
})
console.log('after', after.result?.result?.value)
ws.close()
