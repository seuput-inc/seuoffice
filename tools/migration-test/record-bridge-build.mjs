import { readFileSync, writeFileSync } from 'node:fs'

const { platform, arch, env } = process
const { version } = JSON.parse(readFileSync('apps/shell/package.json', 'utf8'))
const supported =
  (platform === 'win32' && arch === 'x64') || (platform === 'darwin' && arch === 'arm64')
if (!supported) {
  throw new Error('unsupported bridge platform or architecture')
}
writeFileSync(
  'release-files/bridge-build.json',
  JSON.stringify({
    schema: 'seuoffice-electron-bridge-build/v2',
    platform,
    version,
    signing: platform === 'darwin' ? 'ad-hoc' : 'unsigned',
    sourceSha: env.GITHUB_SHA,
    runId: env.GITHUB_RUN_ID,
    runAttempt: env.GITHUB_RUN_ATTEMPT,
  }) + '\n',
  { flag: 'wx' },
)
