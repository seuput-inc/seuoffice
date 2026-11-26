import '@seuoffice/web-platform/globals'
import {
  collabLink,
  historyMode,
  hostEnablesAutoSave,
  hostOffersAi,
  installSandboxFallbacks,
  pageDialogs,
} from '@seuoffice/web-platform'
import { markWebHost, setAppDialogs } from '@seuoffice/ui'
import { installBridges } from './bridge'

installBridges()
installSandboxFallbacks()
setAppDialogs(pageDialogs)
// the AI panel, its buttons and the autosave toggle exist only when the embed host offers them,
// and a page others' edits reach loads every cell font at start: the renderer reads this at start
markWebHost({
  ai: await hostOffersAi(),
  autoSave: await hostEnablesAutoSave(),
  shared: collabLink() !== null || historyMode(),
})
// dynamic so the renderer's modules evaluate after window.desktopApi exists; static imports would hoist above it
await import('../renderer/main')
