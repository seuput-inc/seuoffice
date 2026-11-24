const plain = 'text/plain'
const html = 'text/html'
const notSingleLineText = /[\p{Cc}\p{Cs}\u2028\u2029\ufffc]/u
const notNativeText = /(?![\t\n])[\p{Cc}\p{Cs}\u2028\u2029\ufffc]/u

// Let the browser deliver the clipboard before Studio releases an object or cell selection.
export function collabPasteKey(e) {
  return (
    !e.altKey &&
    (e.ctrlKey || e.metaKey
      ? e.code === 'KeyV' || /^(?:v|ㅍ)$/i.test(e.key)
      : e.shiftKey && e.key === 'Insert')
  )
}

const notices = {
  paste: [
    '공동 편집에서 이 내용을 붙여넣을 수 없습니다. 클립보드는 그대로입니다. 본문이나 표 안에 한 문단의 일반 텍스트를 붙여넣어 보세요.',
    'This paste is unavailable while co-editing. Your clipboard is unchanged. Try a single paragraph of plain text in the body or a table cell.',
  ],
  drop: [
    '공동 편집에서는 끌어 놓기로 내용을 넣을 수 없습니다. 그림은 입력 메뉴에서 파일을 선택한 뒤 위치를 지정하세요.',
    'Drag and drop is unavailable while co-editing. To add a picture, choose it from the Insert menu, then place it.',
  ],
  shortcut: [
    '브라우저에서 클립보드를 열 수 없습니다. 문서에서 Ctrl+V 또는 ⌘V를 눌러 붙여넣으세요.',
    'The browser did not open the clipboard. Press Ctrl+V or ⌘V in the document to paste.',
  ],
  undo: [
    '공동 편집 중에는 이 붙여넣기를 문서 전체 복원으로 되돌릴 수 없습니다. 문서와 클립보드는 그대로입니다.',
    'This paste cannot be undone by restoring the whole shared document. The document and clipboard are unchanged.',
  ],
}

export function collabPasteNotice(locale, toast, reason = 'paste') {
  toast({ message: notices[reason][locale === 'ko' ? 0 : 1] })
}

function source(ih, data, currentMarker, doc) {
  if (
    !data ||
    data.files.length ||
    !data.types.includes(plain) ||
    data.types.some((type) => type !== plain && type !== html) ||
    Array.from(data.items).some(
      (item) => item.kind !== 'string' || (item.type !== plain && item.type !== html),
    )
  )
    return null
  const text = data.getData(plain)
  if (!text || text.length > 256 * 1024) return null
  if (!data.types.includes(html)) {
    if (notSingleLineText.test(text)) return null
    // Browsers can strip the marker from a copied Studio object or paragraph group.
    if (
      doc.hasInternalClipboard() &&
      !doc.clipboardIsSingleText() &&
      doc.getClipboardText() === text
    )
      return null
    return { text, internal: false }
  }
  // The native single-paragraph fragment, not the HTML, proves styles, tabs and soft line breaks.
  if (
    !currentMarker(ih, data.getData(html)) ||
    !doc.clipboardIsSingleText() ||
    notNativeText.test(text) ||
    text.split('\n', 129).length > 128 ||
    JSON.parse(doc.getCollabClipboardText()).text !== text
  )
    return null
  return { text, internal: true }
}

function cellSteps(pos) {
  if (pos.parentParaIndex === undefined) return []
  return pos.cellPath?.length
    ? pos.cellPath
    : [
        {
          controlIndex: pos.controlIndex,
          cellIndex: pos.cellIndex,
          cellParaIndex: pos.cellParaIndex,
        },
      ]
}

function preparePaste(ih, data, currentMarker, insertPlain) {
  const cursor = ih.cursor
  if (
    ih.isFormMode() ||
    ih.isInSeparateCaretSpace() ||
    ih.isComposing ||
    ih.formOverlay ||
    cursor.isInTextBox() ||
    cursor.isInCellSelectionMode() ||
    cursor.isInPictureObjectSelection() ||
    cursor.isInTableObjectSelection()
  )
    return null
  const doc = ih.wasm.borrowDocumentHandle()
  const clip = source(ih, data, currentMarker, doc)
  if (!clip) return null
  const range = cursor.getSelectionOrdered()
  if (!range && cursor.hasSelection()) return null
  const at = range?.start ?? cursor.getPosition()
  const { sectionIndex: sec, charOffset: start } = at
  const steps = cellSteps(at)
  const cell = steps.length > 0
  const para = cell ? at.parentParaIndex : at.paragraphIndex
  const path = JSON.stringify(steps)
  if (
    range &&
    (!ih.isSameTextContainer(at, range.end) ||
      at.sectionIndex !== range.end.sectionIndex ||
      at.paragraphIndex !== range.end.paragraphIndex ||
      at.parentParaIndex !== range.end.parentParaIndex ||
      path !== JSON.stringify(cellSteps(range.end)))
  )
    return null
  const end = range?.end.charOffset ?? start
  const count = end - start
  if (count < 0) return null
  // A scalar deletion cannot stand for two paragraphs, a selected control or its logical caret slot.
  if (count) doc.getCollabLiteralFragment(sec, para, path, start, end)
  if (!clip.internal && !range) return () => insertPlain(clip.text)
  if (clip.internal) doc.getCollabTextPosition(sec, para, path, start, cell ? 'scalar' : 'caret')
  return () => {
    let applied = false
    ih.executeOperation({
      kind: 'snapshot',
      operationType: 'collabPasteText',
      operation: () => {
        let result
        if (count && !cell && !clip.internal) {
          result = doc.replaceText(sec, para, start, count, clip.text)
        } else {
          if (count) {
            const removed = cell
              ? doc.deleteTextInCellByPath(sec, para, path, start, count)
              : doc.deleteText(sec, para, start, count)
            if (JSON.parse(removed).ok !== true) throw new Error('Paste range deletion refused')
          }
          if (!clip.internal) {
            result = cell
              ? doc.insertTextInCellByPath(sec, para, path, start, clip.text)
              : doc.insertTextLogical(sec, para, start, clip.text)
          } else if (!cell) {
            result = doc.pasteInternalTextLogical(sec, para, start)
          } else if (steps.length === 1) {
            const { controlIndex, cellIndex, cellParaIndex } = steps[0]
            result = doc.pasteInternalInCell(
              sec,
              para,
              controlIndex,
              cellIndex,
              cellParaIndex,
              start,
            )
          } else {
            result = doc.pasteInternalInCellByPath(sec, para, path, start)
          }
        }
        if (JSON.parse(result).ok !== true) throw new Error('Text paste refused')
        if (range) cursor.clearSelection()
        applied = true
        return { ...at, charOffset: start + Array.from(clip.text).length }
      },
    })
    if (!applied) throw new Error('The editor did not accept the paste')
  }
}

/** Admission and mutation are synchronous. Never carry a selection into an HTML importer or file decoder. */
export function pasteStudioCollab(ih, data, collab, currentMarker, insertPlain, reject) {
  if (!ih.active) return
  if (collab.locked) return reject()
  let apply
  try {
    apply = preparePaste(ih, data, currentMarker, insertPlain)
  } catch {
    // A missing native proof or an unreadable clipboard cannot authorize even a selection deletion.
  }
  if (!apply) return reject()
  // A failed transaction already displays one error and restores the model and selection.
  return collab.transaction(ih.wasm, apply)
}

/** A native input without a paste event has no trustworthy rich clipboard provenance. Never treat its DOM text as typing. */
export function guardCollabTransferInput(field, reject) {
  const guard = (e) => {
    if (
      e.inputType !== 'insertFromDrop' &&
      e.inputType !== 'deleteByDrag' &&
      e.inputType !== 'insertFromPaste' &&
      e.inputType !== 'insertFromPasteAsQuotation'
    )
      return
    e.stopImmediatePropagation()
    if (e.type === 'beforeinput' && !e.cancelable) return
    e.preventDefault()
    if (e.type === 'input') field.value = ''
    reject()
  }
  field.addEventListener('beforeinput', guard, true)
  field.addEventListener('input', guard, true)
  return () => {
    field.removeEventListener('beforeinput', guard, true)
    field.removeEventListener('input', guard, true)
  }
}
