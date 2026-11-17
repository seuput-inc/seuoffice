/**
 * The editor's page for a document opened only to read it (web embed `?mode=view`): its menus and
 * tool bars go through the editor's chrome API; its rulers have no such switch, and their margin and
 * tab markers are editing handles, so the page hides them and gives the document their room.
 */
const VIEW_ONLY_CSS = `
#ruler-corner, #h-ruler, #v-ruler { display: none !important; }
#editor-area { grid-template-columns: minmax(0, 1fr) auto; grid-template-rows: minmax(0, 1fr); }
#scroll-container { grid-column: 1; grid-row: 1; }
`

export function hideStudioRulers(frame: HTMLIFrameElement): void {
  const page = frame.contentWindow
  const doc = frame.contentDocument
  if (!page || !doc) return
  const sheet = new (page as unknown as { CSSStyleSheet: typeof CSSStyleSheet }).CSSStyleSheet()
  sheet.replaceSync(VIEW_ONLY_CSS)
  doc.adoptedStyleSheets = [...doc.adoptedStyleSheets, sheet]
}
