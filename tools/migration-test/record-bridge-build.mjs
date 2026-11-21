import { readFileSync, writeFileSync } from 'node:fs'

const { platform, arch, env } = process
const { version } = JSON.parse(readFileSync('apps/shell/package.json', 'utf8'))
const signingIdentity = platform === 'darwin' ? env.SEUOFFICE_MAC_SIGN_IDENTITY : null
const supported =
  (platform === 'win32' && arch === 'x64') ||
  (platform === 'darwin' && arch === 'arm64' && /^[A-F0-9]{40}$/.test(signingIdentity || ''))
if (!supported) {
  throw new Error('unsupported bridge platform, architecture or signer')
}
writeFileSync(
  'release-files/bridge-build.json',
  JSON.stringify({
    schema: 'seuoffice-electron-bridge-build/v1',
    platform,
    version,
    signingIdentity,
    sourceSha: env.GITHUB_SHA,
    runId: env.GITHUB_RUN_ID,
    runAttempt: env.GITHUB_RUN_ATTEMPT,
  }) + '\n',
  { flag: 'wx' },
)
