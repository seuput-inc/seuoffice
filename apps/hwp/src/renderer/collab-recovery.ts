import { t } from './i18n/locale'
import './collab-recovery.css'

export type HwpRecoveryFragment =
  { kind: 'text'; text: string } | { kind: 'picture'; blob: Blob; name?: string }

export interface HwpRecovery {
  /**
   * Return true only after inserting at the current cursor. A refusal keeps the item here.
   * `kept` reports a deletion that was not applied; the item offers no reinsertion.
   */
  show<F extends HwpRecoveryFragment>(
    fragment: F,
    reinsert: ((content: F) => boolean | Promise<boolean>) | null,
    reason?: 'deleted' | 'changed' | 'composition' | 'kept',
  ): void
  destroy(): void
}

/** Mount once per local editor. Call show only for a local insertion that lost its target. */
export function mountHwpRecovery(host: HTMLElement): HwpRecovery {
  const doc = host.ownerDocument
  const list = doc.createElement('div')
  list.className = 'hwp-collab-recovery'
  list.setAttribute('aria-live', 'polite')
  host.append(list)
  const previews = new Set<string>()

  return {
    show(fragment, reinsert, reason = 'deleted') {
      if (!list.parentNode) return
      const item = doc.createElement('fieldset')
      const title = doc.createElement('legend')
      title.textContent = t(
        reason === 'composition'
          ? 'collabInterruptedComposition'
          : reason === 'kept'
            ? 'collabKeptTarget'
            : reason === 'changed'
              ? 'collabChangedTarget'
              : fragment.kind === 'text'
                ? 'collabRejectedText'
                : 'collabRejectedPicture',
      )
      item.append(title)
      let url: string | undefined
      if (fragment.kind === 'text') {
        const preview = doc.createElement('div')
        preview.className = 'hwp-collab-recovery-text'
        preview.textContent = fragment.text
        item.append(preview)
      } else {
        const preview = doc.createElement('img')
        url = URL.createObjectURL(fragment.blob)
        previews.add(url)
        preview.src = url
        preview.alt = fragment.name || title.textContent
        item.append(preview)
      }

      const feedback = doc.createElement('div')
      function remove() {
        if (url && previews.delete(url)) URL.revokeObjectURL(url)
        item.remove()
      }
      function button(label: string, act: () => void) {
        const control = doc.createElement('button')
        control.type = 'button'
        control.className = 'hwp-topbar-button'
        control.textContent = label
        control.addEventListener('click', act)
        item.append(control)
      }
      const clipboard = navigator.clipboard
      const canCopy =
        fragment.kind === 'text'
          ? typeof clipboard?.writeText === 'function'
          : typeof clipboard?.write === 'function' &&
            typeof ClipboardItem !== 'undefined' &&
            (ClipboardItem.supports?.(fragment.blob.type) ?? true)
      if (canCopy) {
        button(t('collabCopy'), async () => {
          item.disabled = true
          try {
            if (fragment.kind === 'text') await clipboard.writeText(fragment.text)
            else await clipboard.write([new ClipboardItem({ [fragment.blob.type]: fragment.blob })])
            feedback.textContent = t('collabCopied')
          } catch {
            feedback.textContent = t('collabCopyFailed')
          } finally {
            item.disabled = false
          }
        })
      }
      if (reinsert)
        button(t('collabReinsert'), async () => {
          item.disabled = true
          try {
            if (await reinsert(fragment)) remove()
            else feedback.textContent = t('collabReinsertFailed')
          } catch {
            feedback.textContent = t('collabReinsertFailed')
          } finally {
            item.disabled = false
          }
        })
      button(t('collabDiscard'), remove)
      item.append(feedback)
      list.prepend(item)
      list.scrollTop = 0
    },
    destroy() {
      list.remove()
      list.replaceChildren()
      for (const url of previews) URL.revokeObjectURL(url)
      previews.clear()
    },
  }
}
