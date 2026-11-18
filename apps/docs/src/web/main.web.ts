import '@seuoffice/web-platform/globals'
import {
  hostEnablesAutoSave,
  hostOffersAi,
  installSandboxFallbacks,
  pageDialogs,
  viewMode,
} from '@seuoffice/web-platform'
import { markWebHost, setAppDialogs } from '@seuoffice/ui'
import { installBridges } from './bridge'

// The ribbon starts collapsed (a density value readRibbonDensity understands) so a document
// opened for viewing gets the whole window; the user can still expand it.
if (viewMode()) {
  try {
    localStorage.setItem('aidocs.ribbonCollapsed', 'collapsed')
  } catch {
    /* storage blocked: the ribbon just starts expanded */
  }
}
installBridges()
installSandboxFallbacks()
setAppDialogs(pageDialogs)
// the AI panel, its buttons and the autosave toggle exist only when the embed host offers them: the renderer reads this at start
markWebHost({ ai: await hostOffersAi(), autoSave: await hostEnablesAutoSave() })
// dynamic so the renderer's modules evaluate after window.desktop exists; static imports would hoist above it
await import('../renderer/main')
