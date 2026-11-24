export { WebUnsupportedError, unsupported } from './errors'
export { installSandboxFallbacks } from './sandbox-fallbacks'
export { pageDialogs, showPageDialog } from './page-dialogs'
export { createWebBridge, installWebBridge } from './bridge'
export {
  documentByPath,
  editMode,
  viewMode,
  waitForDocument,
  webLanguage,
  webRequestedLanguage,
  type WebDocument,
} from './document-store'
export {
  historyMode,
  hostEnablesAutoSave,
  onHostSaveRequest,
  onHostThumbnailRequest,
  registerCollabCapable,
  reportDirty,
  reportError,
  reportOpened,
  saveReason,
  saveToHost,
  type EmbedOpenErrorCode,
  type SaveReason,
} from './embed-channel'
export {
  collabLink,
  reportCollabStatus,
  type CollabLink,
  type CollabLinkEvent,
  type CollabStatus,
} from './collab-link'
export { joinPageCollab, type PageCollab } from './collab-page'
export { presenceLabel, type PresenceMark } from './presence'
export { createAiStreamBridge, hostOffersAi, webAiSettings, type AiStreamBridge } from './ai-relay'
