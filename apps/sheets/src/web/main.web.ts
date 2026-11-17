import '@seuoffice/web-platform/globals'
import { hostOffersAi, installSandboxFallbacks } from '@seuoffice/web-platform'
import { markWebAiOffer } from '@seuoffice/ui'
import { installBridges } from './bridge'

installBridges()
installSandboxFallbacks()
// the AI panel and its buttons exist only when the embed host offers AI: the renderer reads this at start
markWebAiOffer(await hostOffersAi())
// dynamic so the renderer's modules evaluate after window.desktopApi exists; static imports would hoist above it
await import('../renderer/main')
