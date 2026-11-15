interface EngineServiceWindow extends Window {
  __SEUOFFICE_ENGINE_TOKEN__: string
  __TAURI_INTERNALS__: {
    invoke(command: string, args: Record<string, unknown>): Promise<unknown>
  }
}

export type NativeCallback = (operation: string, input: unknown) => Promise<unknown>

export function createRequestCallbacks(
  id: number,
  name: string,
  nonce: string,
  signal: AbortSignal,
): NativeCallback {
  let callbackId = 0
  const service = window as unknown as EngineServiceWindow
  const token = service.__SEUOFFICE_ENGINE_TOKEN__
  return async (operation, input) => {
    signal.throwIfAborted()
    if (!Number.isSafeInteger(++callbackId)) throw new Error('engine callback identities exhausted')
    const result = await service.__TAURI_INTERNALS__.invoke('engine_host_callback', {
      token,
      id,
      name,
      nonce,
      callbackId,
      operation,
      input,
    })
    signal.throwIfAborted()
    return result
  }
}
