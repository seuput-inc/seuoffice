const { path, port } = JSON.parse(await new Response(Deno.stdin.readable).text())
const checks = {
  credential: () => Deno.env.get('HISTORY_TEST_CREDENTIAL'),
  read: () => Deno.readTextFile(path),
  write: () => Deno.writeTextFile(path, 'changed'),
  socket: () => Deno.connect({ hostname: '127.0.0.1', port }),
  process: () => new Deno.Command('/bin/echo', { args: ['spawned'] }).output(),
}
const result = {}
for (const [key, check] of Object.entries(checks)) {
  try {
    await check()
    result[key] = 'granted'
  } catch (error) {
    result[key] = error.name
  }
}
for (const name of ['ffi', 'sys', 'import'])
  result[name] = (await Deno.permissions.query({ name })).state
console.log(JSON.stringify(result))
