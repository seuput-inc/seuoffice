import { CLIPBOARD_HTML_MODULE } from '../studio-plugin/clipboard-html.mjs'

export const COLLAB_PASTE_PATCHES = {
  'studio-collab-paste': {
    file: /[\\/]rhwp-studio[\\/]src[\\/]engine[\\/]input-handler-keyboard\.ts$/,
    edits: [
      {
        find: "import { isBodyControl } from './picture-hit-policy';",
        replace: [
          "import { isBodyControl } from './picture-hit-policy';",
          "import { studioCollab } from '@seuoffice/studio-collab-input';",
          "import { getLocale } from '@/i18n/core.ts';",
          "import { showToast } from '@/ui/toast';",
          `import { collabPasteKey, collabPasteNotice, pasteStudioCollab } from '${CLIPBOARD_HTML_MODULE}';`,
        ].join('\n'),
      },
      {
        find: 'export function onKeyDown(this: any, e: KeyboardEvent): void {\n  if (!this.active) return;',
        replace: [
          'export function onKeyDown(this: any, e: KeyboardEvent): void {',
          '  if (!this.active) return;',
          '  if (studioCollab && collabPasteKey(e)) return;',
        ].join('\n'),
      },
      {
        find: 'export function onPaste(this: any, e: ClipboardEvent): void {\n  if (!this.active) return;\n  e.preventDefault();',
        replace: [
          'export function onPaste(this: any, e: ClipboardEvent): void {',
          '  if (!this.active) return;',
          '  e.preventDefault();',
          '  if (studioCollab) return pasteStudioCollab(this, e.clipboardData, studioCollab,',
          '    hasCurrentRhwpClipboardMarker, (text: string) => pastePlainText.call(this, text, false),',
          '    () => collabPasteNotice(getLocale(), showToast));',
        ].join('\n'),
      },
    ],
  },
  'studio-collab-paste-input': {
    file: /[\\/]rhwp-studio[\\/]src[\\/]engine[\\/]input-handler\.ts$/,
    edits: [
      {
        find: "import { t } from '@/i18n/index';",
        replace: `import { getLocale, t } from '@/i18n/index';\nimport { collabPasteNotice, guardCollabTransferInput } from '${CLIPBOARD_HTML_MODULE}';`,
      },
      {
        find: '  private onPasteBound: (e: ClipboardEvent) => void;',
        replace:
          '  private disposeTransferInput?: () => void;\n  private onPasteBound: (e: ClipboardEvent) => void;',
      },
      {
        find: '    this.onIMEWindowBlurBound = () => this._imeNavigationGuard.reset();',
        replace: [
          '    this.onIMEWindowBlurBound = () => this._imeNavigationGuard.reset();',
          '    if (studioCollab) this.disposeTransferInput = guardCollabTransferInput(this.textarea,',
          '      () => collabPasteNotice(getLocale(), showToast));',
        ].join('\n'),
      },
      {
        find: "    this.textarea.removeEventListener('paste', this.onPasteBound);",
        replace:
          "    this.disposeTransferInput?.();\n    this.textarea.removeEventListener('paste', this.onPasteBound);",
      },
      {
        find: "    return document.execCommand('paste');",
        replace: [
          "    if (!studioCollab) return document.execCommand('paste');",
          '    let delivered = false;',
          '    const received = () => { delivered = true; };',
          "    this.textarea.addEventListener('paste', received, true);",
          '    try {',
          "      const pasted = document.execCommand('paste');",
          "      if (!pasted && !delivered) collabPasteNotice(getLocale(), showToast, 'shortcut');",
          '      return pasted || delivered;',
          '    } finally {',
          "      this.textarea.removeEventListener('paste', received, true);",
          '    }',
        ].join('\n'),
      },
      {
        find: '  private handleUndo(): void {',
        replace: [
          '  private handleUndo(): void {',
          "    if (studioCollab && this.history.peekUndoTop()?.type === 'snapshot:collabPasteText')",
          "      return collabPasteNotice(getLocale(), showToast, 'undo');",
        ].join('\n'),
      },
    ],
  },
  'studio-collab-paste-drop': {
    file: /[\\/]rhwp-studio[\\/]src[\\/]main\.ts$/,
    edits: [
      {
        find: "import { setLinkHostHandler } from './core/link-navigation';",
        replace: `import { setLinkHostHandler } from './core/link-navigation';\nimport { collabPasteNotice } from '${CLIPBOARD_HTML_MODULE}';`,
      },
      {
        find: '    const file = e.dataTransfer?.files[0];\n    if (!file) return;',
        replace: [
          '    const file = e.dataTransfer?.files[0];',
          '    if (!file) {',
          "      if (studioCollab && e.dataTransfer?.types.length) collabPasteNotice(getLocale(), showToast, 'drop');",
          '      return;',
          '    }',
        ].join('\n'),
      },
      {
        find: '      if (!await showDropConfirmDialog(file.name)) return;',
        replace: [
          "      if (studioCollab) return collabPasteNotice(getLocale(), showToast, 'drop');",
          '      if (!await showDropConfirmDialog(file.name)) return;',
        ].join('\n'),
      },
    ],
  },
  'studio-collab-paste-transposed': {
    file: /[\\/]rhwp-studio[\\/]src[\\/]command[\\/]commands[\\/]table\.ts$/,
    edits: [
      {
        find: "import { TableCreateDialog } from '@/ui/table-create-dialog';",
        replace: [
          "import { TableCreateDialog } from '@/ui/table-create-dialog';",
          "import { studioCollab } from '@seuoffice/studio-collab-input';",
          "import { showToast } from '@/ui/toast';",
          `import { collabPasteNotice } from '${CLIPBOARD_HTML_MODULE}';`,
        ].join('\n'),
      },
      {
        find: '    canExecute: (ctx) => ctx.hasDocument && ctx.hasTableTransposeClipboard,\n    execute(services) {',
        replace: [
          '    canExecute: (ctx) => ctx.hasDocument && ctx.hasTableTransposeClipboard,',
          '    execute(services) {',
          '      if (studioCollab) return collabPasteNotice(getLocale(), showToast);',
        ].join('\n'),
      },
    ],
  },
}
