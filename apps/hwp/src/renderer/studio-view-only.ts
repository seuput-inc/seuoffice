/**
 * The editor's page for a document opened only to read it (web embed `?mode=view`): its menus and
 * tool bars go through the editor's chrome API. What has no such switch is hidden here: the rulers,
 * whose margin and tab markers are editing handles (the document gets their room), the text caret,
 * the handles around a clicked picture or table, the line that follows a table border, and the
 * status bar's insert/overwrite indicator.
 */
const VIEW_ONLY_CSS = `
#ruler-corner, #h-ruler, #v-ruler { display: none !important; }
#editor-area { grid-template-columns: minmax(0, 1fr) auto; grid-template-rows: minmax(0, 1fr); }
#scroll-container { grid-column: 1; grid-row: 1; }
#scroll-container .caret, #scroll-container .caret-composition { display: none !important; }
.table-object-layer, .table-resize-layer { display: none !important; }
#sb-mode, #ieum-sb-char-count-divider { display: none !important; }
`

export function hideStudioEditingMarks(frame: HTMLIFrameElement): void {
  const page = frame.contentWindow
  const doc = frame.contentDocument
  if (!page || !doc) return
  const sheet = new (page as unknown as { CSSStyleSheet: typeof CSSStyleSheet }).CSSStyleSheet()
  sheet.replaceSync(VIEW_ONLY_CSS)
  doc.adoptedStyleSheets = [...doc.adoptedStyleSheets, sheet]
}
