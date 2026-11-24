import {
  editMode,
  joinPageCollab,
  reportError,
  webLanguage,
  type PageCollab,
  type WebDocument,
} from '@seuoffice/web-platform'
import type { HwpStudioControls } from '../../studio-plugin/collab-input.mjs'
import type { HwpPresence } from '../../studio-plugin/collab-presence.mjs'
import { mountHwpRecovery, type HwpRecovery } from '../renderer/collab-recovery'
import { t } from '../renderer/i18n/locale'
import { hwpFormatOfPath } from '../shared/ipc'
import {
  createHwpCollabAdapter,
  prepareHwpCollab,
  type HwpCollabBinding,
  type PreparedHwpCollab,
} from './collab-adapter'

interface StudioMount {
  host: HTMLElement
  frame: HTMLIFrameElement
  recovery: HwpRecovery
  draftReason(): string | null
  notice(message: string): void
  controls?: HwpStudioControls
  binding?: HwpCollabBinding
  presence?: HwpPresence
  buffered: boolean
}

/** Save what this editor shows on this computer, the way left once the session stopped. */
async function downloadLocal(binding: HwpCollabBinding): Promise<void> {
  const { fileName, bytes } = await binding.adapter.produceFile()
  const url = URL.createObjectURL(new Blob([bytes as BlobPart]))
  const link = document.createElement('a')
  link.href = url
  link.download = fileName
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 60_000)
}

/** A host's bytes must pass admission before they can seed an epoch. A late joiner validates the epoch base too. */
export async function openHwpCollabPage(
  doc: WebDocument,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  const format = hwpFormatOfPath(doc.fileName)
  let file: PreparedHwpCollab | undefined
  let opening: 'source' | 'session' = 'source'
  let joined: PageCollab<symbol> | null = null
  const editing = editMode()
  let mount: StudioMount | undefined
  let disposed = false
  const loaded = Symbol('loaded HWP editor')
  const activeMount = () =>
    !disposed && mount?.host.isConnected && mount.frame.parentElement === mount.host
      ? mount
      : undefined
  const connect: NonNullable<Window['seuHwpCollabConnect']> = (source, next) => {
    const active = activeMount()
    if (!active || active.controls || active.frame.contentWindow !== source) {
      throw new Error('Not the active HWP editor frame')
    }
    active.controls = next
    next.setEditable(false)
    return {
      transaction(doc, action) {
        if (activeMount() !== active || !active.binding)
          throw new Error('The HWP collaboration editor is not ready')
        try {
          return active.binding.callbacks.transaction(doc, action)
        } catch (error) {
          const reason = error instanceof Error ? error.message : String(error)
          active.notice(
            webLanguage() === 'ko'
              ? `공동편집에 반영하지 않았습니다. ${reason}`
              : `This edit was not shared. ${reason}`,
          )
          throw error
        }
      },
      onMutation(...args) {
        if (!editing || activeMount() !== active || !active.binding) {
          throw new Error('The HWP collaboration editor is not editable')
        }
        return active.binding.callbacks.onMutation(...args)
      },
      onBuffer(next) {
        if (activeMount() !== active) return
        active.buffered = next
        active.binding?.callbacks.onBuffer(next)
      },
      onDraftLost(text) {
        if (activeMount() === active) active.binding?.callbacks.onDraftLost?.(text)
      },
      onCaretLost() {
        if (activeMount() === active) active.binding?.callbacks.onCaretLost?.()
      },
    }
  }
  const requireNoDraft = () => {
    const active = activeMount()
    if (!active?.binding) throw new Error('The HWP collaboration editor is not ready')
    const reason = active.draftReason()
    if (reason) throw new Error(reason)
    if (active.buffered) {
      throw new Error(
        webLanguage() === 'ko'
          ? '조합 중인 글자를 확정한 뒤 다시 시도하세요.'
          : 'Finish the current text composition and try again.',
      )
    }
  }
  const renderer: NonNullable<Window['seuHwpCollabPage']> = {
    mount(container, element, { recoveryHost, draftReason, notice }) {
      if (
        disposed ||
        mount ||
        !(element instanceof HTMLIFrameElement) ||
        element.parentElement !== container
      ) {
        return
      }
      mount = {
        host: container,
        frame: element,
        draftReason,
        notice,
        recovery: mountHwpRecovery(recoveryHost),
        buffered: false,
      }
    },
    async loaded(element) {
      const active = activeMount()
      if (!active?.controls || element !== active.frame || !joined) {
        throw new Error('The HWP collaboration frame did not connect')
      }
      try {
        await joined.attach(loaded)
      } catch (error) {
        // A presence hook can fail after the session attaches but before the host sees opened.
        if (!disposed)
          reportError('collab_unavailable', error instanceof Error ? error.message : String(error))
        throw error
      }
      // joinPageCollab reports construction errors to the host rather than rejecting attach.
      if (!active.binding) throw new Error('The HWP collaboration editor could not attach')
    },
    async save(automatic) {
      requireNoDraft()
      if (!editing || !joined) throw new Error('This document cannot be saved here')
      await joined.save({ reason: automatic ? 'auto' : 'user', skipWhenSaved: true })
    },
    unsaved: () => mount?.buffered === true || window.seuCollab?.unsaved?.() === true,
    fail(reason) {
      if (disposed || window.seuHwpCollabPage !== renderer) return
      window.seuCollab?.fail?.(reason)
      dispose()
    },
  }
  function dispose() {
    if (disposed) return
    disposed = true
    signal?.removeEventListener('abort', dispose)
    window.removeEventListener('pagehide', dispose)
    if (window.seuHwpCollabPage === renderer) {
      delete window.seuHwpCollabPage
      document.documentElement.removeAttribute('data-studio-collab')
    }
    if (window.seuHwpCollabConnect === connect) delete window.seuHwpCollabConnect
    joined?.dispose?.()
    const closed = mount
    mount = undefined
    closed?.controls?.setEditable(false)
    closed?.presence?.dispose()
    closed?.recovery.destroy()
    closed?.binding?.dispose()
    file?.dispose()
    file = undefined
  }
  try {
    if (!format) throw new Error('Only HWP and HWPX files can be opened for co-editing.')
    signal?.throwIfAborted()
    file = await prepareHwpCollab(format, doc.bytes)
    signal?.throwIfAborted()
    signal?.addEventListener('abort', dispose, { once: true })
    window.addEventListener('pagehide', dispose, { once: true })
    opening = 'session'
    const offered = file.bytes
    joined = await joinPageCollab({
      format: { name: format, schema: 1 },
      fileName: doc.fileName,
      bytes: offered,
      adapter(token: symbol) {
        const active = activeMount()
        if (token !== loaded || !active?.controls || !file) {
          throw new Error('No loaded HWP collaboration editor')
        }
        active.binding = createHwpCollabAdapter({
          file,
          fileName: doc.fileName,
          controls: active.controls,
          onLost: active.recovery.show,
          notice: active.notice,
        })
        if (active.buffered) active.binding.callbacks.onBuffer(true)
        const { adapter } = active.binding
        // A view-mode embed may have an edit-capable server credential. Replay still needs the raw document.
        return editing ? adapter : { ...adapter, setEditable: () => adapter.setEditable(false) }
      },
      status(next) {
        const active = activeMount()
        const binding = active?.binding
        if (next.phase !== 'failed' || !binding) return
        // The server answers a history it cannot extend with too_large (adapter) or schema_mismatch.
        const history = next.failure === 'adapter' || next.failure === 'schema_mismatch'
        active.recovery.stopped(t(history ? 'collabHistoryStopped' : 'collabStopped'), () =>
          downloadLocal(binding),
        )
      },
      presence: (_token, publish) => {
        const active = activeMount()
        if (!active?.controls) throw new Error('The HWP editor was closed')
        return (active.presence = active.controls.bindPresence(publish))
      },
      hostSave: (save) => {
        requireNoDraft()
        return save()
      },
      commitDrafts: requireNoDraft,
    })
    if (!joined) throw new Error('The HWP collaboration session did not open')
    if (disposed) {
      joined.dispose?.()
      throw new Error('The HWP editor was closed')
    }
    const base = joined.baseBytes
    if (
      base !== offered &&
      (base.length !== offered.length || !base.every((b, i) => b === offered[i]))
    ) {
      opening = 'source'
      file.dispose()
      file = await prepareHwpCollab(format, base)
      if (disposed) {
        file.dispose()
        file = undefined
        throw new Error('The HWP editor was closed')
      }
    }
    window.seuHwpCollabConnect = connect
    window.seuHwpCollabPage = renderer
    document.documentElement.setAttribute('data-studio-collab', '')
    return file.bytes
  } catch (cause) {
    if (disposed || signal?.aborted) {
      dispose()
      throw cause
    }
    const reason = cause instanceof Error ? cause.message : String(cause)
    const ko = webLanguage() === 'ko'
    const explanation =
      opening === 'source'
        ? ko
          ? '이 파일의 내용을 두 사람이 고친 뒤에도 빠짐없이 저장할 수 있는지 확인하지 못했습니다. 뜻을 알 수 없는 문서 정보가 있는 파일은 공동편집과 이력을 열지 않습니다. 원본은 변경하지 않았습니다. 원본과 저장 결과는 각각 2 MiB 이하여야 합니다.'
          : 'The editor could not verify that every part of this file survives co-editing. Files with document information it cannot interpret cannot join or create history. The original is unchanged. The input and saved file must each be at most 2 MiB.'
        : ko
          ? '공동편집 또는 기록 세션을 열지 못했습니다.'
          : 'The collaboration or history session could not be opened.'
    const message = `${explanation} ${reason}`
    // A source that cannot survive saving must not trigger the host's automatic single-writer fallback.
    if (opening === 'source') reportError('unsupported', message)
    else if (joined && window.seuCollab?.fail) window.seuCollab.fail(message)
    else reportError('collab_unavailable', message)
    dispose()
    throw new Error(message, { cause })
  }
}
