// Selected only by the build-only bridge dispatch. The release config never loads this file.
const { readFileSync } = require('node:fs')
const { env, argv } = process
if (
  env.GITHUB_EVENT_NAME !== 'workflow_dispatch' ||
  env.GITHUB_REPOSITORY !== 'seuput-inc/seuoffice' ||
  env.GITHUB_REF !== 'refs/heads/integration/tauri-first-candidate' ||
  argv.at(-2) !== '--publish' ||
  argv.at(-1) !== 'never'
) {
  throw new Error('migration-seam packaging requires a nonpublishing candidate dispatch')
}
const { inputs } = JSON.parse(readFileSync(env.GITHUB_EVENT_PATH, 'utf8'))
if (
  String(inputs.migration_seam) !== 'true' ||
  String(inputs.release) !== 'false' ||
  !/^\d+\.\d+\.\d+$/.test(inputs.bridge_version) ||
  inputs.bridge_version !== require('./package.json').version ||
  inputs.candidate_version ||
  inputs.next_version
) {
  throw new Error('migration-seam packaging requires its own explicit bridge dispatch')
}

const release = require('./electron-builder.seuoffice.cjs')
const { channel } = release.publish[0]
const suffix = `${channel === 'stable' ? '-stable' : ''}-migration-seam.\${ext}`
module.exports = {
  ...release,
  publish: [{ provider: 'generic', url: 'http://127.0.0.1:8765/', channel }],
  extraMetadata: { ...release.extraMetadata, seuofficeBuildProfile: 'migration-seam' },
  nsis: { ...release.nsis, artifactName: `\${productName}-Setup-\${version}${suffix}` },
  dmg: { ...release.dmg, artifactName: `\${productName}-\${version}-\${arch}${suffix}` },
  mac: { ...release.mac, artifactName: `\${productName}-\${version}-\${arch}-mac${suffix}` },
}
