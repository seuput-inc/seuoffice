import { contextBridge, ipcRenderer } from 'electron'
import type { IpcRendererEvent } from 'electron'
import type { PdfPasswordUiState, PdfPasswordWindowApi } from '../shared/pdf-password-api'
import { PDF_PASSWORD_CHANNELS } from '../shared/pdf-password-api'

let generation: string | undefined

const api: PdfPasswordWindowApi = {
  async getState() {
    const result: unknown = await ipcRenderer.invoke(PDF_PASSWORD_CHANNELS.getState)
    const state = (result ?? null) as PdfPasswordUiState | null
    if (state && generation && BigInt(state.generation) < BigInt(generation)) return null
    if (state) generation = state.generation
    return state
  },
  submit(password) {
    void ipcRenderer.invoke(PDF_PASSWORD_CHANNELS.submit, password, generation)
  },
  cancel() {
    void ipcRenderer.invoke(PDF_PASSWORD_CHANNELS.cancel, generation)
  },
  onState(handler) {
    const listener = (_event: IpcRendererEvent, state: PdfPasswordUiState) => {
      generation = state.generation
      handler(state)
    }
    ipcRenderer.on(PDF_PASSWORD_CHANNELS.changed, listener)
    return () => ipcRenderer.removeListener(PDF_PASSWORD_CHANNELS.changed, listener)
  },
}

contextBridge.exposeInMainWorld('aiOfficePdfPassword', api)
