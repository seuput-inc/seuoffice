/// <reference types="vite/client" />

import type { ProjectApi, ProjectFolderApi } from '@seuoffice/project-store'
import type { SplitPaneBridge } from '@seuoffice/ui'
import type { HwpApi } from '../shared/ipc'

declare global {
  interface Window {
    /** project folders (decision 0088); absent outside the shell or with an older preload */
    projectFolderApi?: ProjectFolderApi
    hwpApi: HwpApi
    /** Present only after the web page admitted the host's co-editing or history document. */
    seuHwpCollabPage?: {
      mount(
        host: HTMLElement,
        frame: Element | null,
        ui: {
          recoveryHost: HTMLElement
          draftReason(): string | null
          notice(message: string): void
        },
      ): void
      loaded(frame: HTMLIFrameElement): Promise<void>
      save(automatic: boolean): Promise<void>
      unsaved(): boolean
      fail(reason: string): void
    }
    /** the shell's chat history of documents; absent where no shell registered it */
    projectApi?: Pick<ProjectApi, 'resolveChat' | 'appendChat' | 'loadChat' | 'rebindChat'>
    /** the shell's split panes; read through @seuoffice/ui's split-pane-role helpers */
    aiOfficeSplit?: SplitPaneBridge
  }
}

export {}
