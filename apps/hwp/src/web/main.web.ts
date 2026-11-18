import '@seuoffice/web-platform/globals'
import {
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
// the AI panel, its buttons and the autosave toggle exist only when the embed host offers them: the renderer reads this at start
markWebHost({ ai: await hostOffersAi(), autoSave: await hostEnablesAutoSave() })
// dynamic so the renderer's modules evaluate after window.hwpApi exists; static imports would hoist above it
await import('../renderer/main')
