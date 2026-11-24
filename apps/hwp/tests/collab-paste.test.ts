// @vitest-environment jsdom
import { existsSync, readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import ts from 'typescript'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { applyPatch } from '../scripts/rhwp-patches.mjs'
import { applyHwpOp, captureHwpUndo, type HwpOp } from '../src/shared/collab-editor-ops'
import { releaseHwpUndo } from '../src/shared/collab-text-undo'
import {
  createHwpCollabAdapter,
  prepareHwpCollab,
  type PreparedHwpCollab,
} from '../src/web/collab-adapter'
import { wrapHwpMutations } from '../studio-plugin/collab-capture.mjs'
import {
  collabPasteKey,
  collabPasteNotice,
  guardCollabTransferInput,
  pasteStudioCollab,
} from '../studio-plugin/collab-paste.mjs'

let file: PreparedHwpCollab
let table: { paraIdx: number; controlIdx: number }
beforeAll(async () => {
  const dir = dirname(createRequire(import.meta.url).resolve('@rhwp/core'))
  await init({ module_or_path: readFileSync(join(dir, 'rhwp_bg.wasm')) })
  const seed = HwpDocument.createEmpty()
  try {
    seed.createBlankDocument()
    seed.splitParagraph(0, 0, seed.getParagraphLength(0, 0))
    seed.insertText(0, 1, 0, '가🙂나')
    seed.splitParagraph(0, 1, 3)
    seed.insertText(0, 2, 0, 'ABCD')
    seed.splitParagraph(0, 2, 4)
    table = JSON.parse(seed.createTable(0, 3, 0, 1, 1))
    seed.applyCharFormat(0, 1, 1, 2, '{"italic":true}')
    file = await prepareHwpCollab('hwpx', seed.exportHwpx())
  } finally {
    seed.free()
  }
})
afterAll(() => file?.dispose())
const cleanup: (() => void)[] = []
afterEach(() => {
  while (cleanup.length) cleanup.pop()!()
  vi.restoreAllMocks()
})

type Cell = { controlIndex: number; cellIndex: number; cellParaIndex: number }
type Pos = {
  sectionIndex: number
  paragraphIndex: number
  charOffset: number
  parentParaIndex?: number
  cellPath?: Cell[]
} & Partial<Cell>
const at = (n: number, paragraph = 2): Pos => ({
  sectionIndex: 0,
  paragraphIndex: paragraph,
  charOffset: n,
})
const marker = '<!--rhwp-studio-clipboard:current-->'
const current = (ih: { rhwpClipboardToken: string }, html: string) =>
  html.includes(`<!--rhwp-studio-clipboard:${ih.rhwpClipboardToken}-->`)
interface ClipboardContents {
  types: string[]
  files: File[]
  items: { kind: string; type: string }[]
  getData(type: string): string
}
function transfer(formats: Record<string, string>, files: File[] = []) {
  return {
    types: Object.keys(formats).concat(files.length ? ['Files'] : []),
    files,
    items: [
      ...Object.keys(formats).map((type) => ({ kind: 'string', type })),
      ...files.map((f) => ({ kind: 'file', type: f.type, getAsFile: () => f })),
    ],
    getData: (type: string) => formats[type] ?? '',
    setData: vi.fn(),
    clearData: vi.fn(),
  }
}
const plain = (text: string) => transfer({ 'text/plain': text })
const marked = (text: string) =>
  transfer({ 'text/plain': text, 'text/html': marker + '<p>copy</p>' })

function editor() {
  const doc = new HwpDocument(file.bytes)
  const bound = createHwpCollabAdapter({
    file,
    fileName: 'paste.hwpx',
    onLost: vi.fn(),
    notice: vi.fn(),
    controls: {
      withDocument: (fn) => fn(doc),
      replay: (fn) => fn(doc, null),
      setEditable: vi.fn(),
      bindPresence: vi.fn(),
    },
  })
  cleanup.push(() => {
    bound.dispose()
    doc.free()
  })
  const submit = vi.fn((_ops: HwpOp[]) => {})
  bound.adapter.listen(submit)
  bound.adapter.setEditable(true)
  const wrapped = wrapHwpMutations(doc, bound.callbacks.onMutation)
  doc.copySelection(0, 1, 0, 1, 3)
  let head = at(3),
    range: { start: Pos; end: Pos } | null = { start: at(1), end: head }
  const cursor = {
    getPosition: () => head,
    getSelectionOrdered: () => range,
    hasSelection: () => !!range,
    clearSelection: () => {
      range = null
    },
    isInTextBox: () => false,
    isInCellSelectionMode: () => false,
    isInPictureObjectSelection: () => false,
    isInTableObjectSelection: () => false,
  }
  const ih = {
    active: true,
    rhwpClipboardToken: 'current',
    cursor,
    wasm: { borrowDocumentHandle: () => wrapped },
    isFormMode: () => false,
    isInSeparateCaretSpace: () => false,
    // The admission boundary must prove the actual paragraph even if a container comparison is coarser.
    isSameTextContainer: () => true,
    executeOperation: ({ operation }: { operation: () => Pos }) => {
      head = operation()
    },
  }
  const collab = {
    locked: false,
    transaction: vi.fn((_: unknown, apply: () => void) => bound.callbacks.transaction(doc, apply)),
  }
  const toast = vi.fn()
  const insert = vi.fn((text: string) => {
    wrapped.insertTextLogical(head.sectionIndex, head.paragraphIndex, head.charOffset, text)
    head = { ...head, charOffset: head.charOffset + [...text].length }
  })
  return {
    ih,
    doc,
    collab,
    submit,
    toast,
    insert,
    select(start: Pos | null, end: Pos) {
      head = end
      range = start && { start, end }
    },
    paste(data: ClipboardContents | null, locale = 'ko') {
      return pasteStudioCollab(ih, data, collab, current, insert, () =>
        collabPasteNotice(locale, toast),
      )
    },
  }
}

it('refuses opaque formats, files, empty text and paragraph groups without touching either clipboard or selection', () => {
  const e = editor()
  const before = e.doc.getCollabFingerprint(),
    selection = e.ih.cursor.getSelectionOrdered()
  const payloads = [
    null,
    transfer({}),
    plain(''),
    plain('한\r\n글'),
    plain('한\u2029글'),
    plain('한\ufffc글'),
    plain('\ud800'),
    transfer({
      'text/plain': '대체',
      'text/html': '<img src="file:///picture.png">',
      'text/rtf': '{\\rtf1}',
    }),
    transfer({ 'text/plain': '대체', 'text/html': '<!--[data-hwpjson] {"sections":[]}-->' }),
    transfer({
      'text/plain': '가🙂나',
      'text/html': '<!--rhwp-studio-clipboard:other--><b>가🙂나</b>',
    }),
    marked('different'),
    transfer({ 'text/plain': '대체', 'application/x-opaque': '' }),
    transfer({ 'text/plain': '그림' }, [new File(['png'], 'copy.png', { type: 'image/png' })]),
  ]
  for (const [index, data] of payloads.entries()) {
    e.paste(data, index % 2 ? 'en' : 'ko')
    expect(e.toast).toHaveBeenLastCalledWith({
      message: expect.stringContaining(index % 2 ? 'clipboard is unchanged' : '클립보드는 그대로'),
    })
    expect(e.toast).toHaveBeenCalledTimes(index + 1)
    expect(e.ih.cursor.getSelectionOrdered()).toEqual(selection)
    expect(e.doc.getCollabFingerprint()).toBe(before)
    expect(e.doc.getClipboardText()).toBe('가🙂나')
    if (data) {
      expect(data.clearData).not.toHaveBeenCalled()
      expect(data.setData).not.toHaveBeenCalled()
    }
  }
  e.doc.copySelection(0, 1, 0, 2, 2)
  e.paste(marked(e.doc.getClipboardText()))
  expect(e.doc.clipboardIsSingleText()).toBe(false)
  e.doc.copyControl(0, table.paraIdx, '', table.controlIdx)
  expect(e.doc.clipboardHasControl()).toBe(true)
  const display = e.doc.getClipboardText()
  for (const data of [plain(display), marked(display)]) e.paste(data)
  expect(e.doc.getClipboardText()).toBe(display)
  expect(e.doc.clipboardHasControl()).toBe(true)
  expect(e.ih.cursor.getSelectionOrdered()).toEqual(selection)
  expect(e.doc.getCollabFingerprint()).toBe(before)
  expect(e.collab.transaction).not.toHaveBeenCalled()
  expect(e.submit).not.toHaveBeenCalled()
})

it('does not turn matching char offsets across body or nested cell paragraphs into a collapsed paste', () => {
  const e = editor(),
    text = plain('새')
  const outer = { controlIndex: 0, cellIndex: 0, cellParaIndex: 0 }
  const cell = (root: number, path: Cell[]): Pos => ({
    ...at(1),
    parentParaIndex: root,
    cellPath: path,
    ...path.at(-1)!,
  })
  const selections = [
    [at(1, 1), at(1, 2)],
    [at(0, 1), at(2, 2)],
    [cell(3, [outer]), cell(4, [outer])],
    [cell(3, [outer, outer]), cell(3, [outer, { ...outer, cellParaIndex: 1 }])],
    [cell(3, [outer, outer]), cell(3, [{ ...outer, cellParaIndex: 1 }, outer])],
  ]
  const proof = vi.spyOn(e.doc, 'getCollabLiteralFragment')
  const before = e.doc.getCollabFingerprint()
  for (const [start, end] of selections) {
    e.select(start!, end!)
    e.paste(text)
    expect(e.ih.cursor.getSelectionOrdered()).toEqual({ start, end })
  }
  expect(e.toast).toHaveBeenCalledTimes(selections.length)
  expect(proof).not.toHaveBeenCalled()
  expect(e.collab.transaction).not.toHaveBeenCalled()
  expect(e.doc.getCollabFingerprint()).toBe(before)
})

it('rejects a selection containing an inline object, an object or cell block selection, and a missing range proof before deletion', () => {
  const e = editor()
  vi.spyOn(e.ih.cursor, 'isInPictureObjectSelection').mockReturnValueOnce(true)
  e.paste(plain('새'))
  vi.spyOn(e.ih.cursor, 'isInCellSelectionMode').mockReturnValueOnce(true)
  e.paste(plain('새'))
  e.doc.insertFootnote(0, 2, 2)
  const before = e.doc.getCollabFingerprint()
  e.paste(plain('새'))
  Object.defineProperty(e.doc, 'getCollabLiteralFragment', { value: undefined })
  e.paste(plain('새'))
  expect(e.toast).toHaveBeenCalledTimes(4)
  expect(e.collab.transaction).not.toHaveBeenCalled()
  expect(e.doc.getCollabFingerprint()).toBe(before)
  expect(e.ih.cursor.getSelectionOrdered()).toEqual({ start: at(1), end: at(3) })
})

it('commits a literal selection replacement as one operation; its retained inverse preserves a later peer insertion', () => {
  const e = editor(),
    peer = e.doc.forkCollabDocument()
  try {
    e.paste(plain('한🙂'))
    expect(e.submit).toHaveBeenCalledTimes(1)
    const ops = e.submit.mock.calls[0]![0]
    expect(ops).toHaveLength(1)
    expect(e.ih.cursor.getPosition()).toEqual(at(3))
    expect(e.ih.cursor.getSelectionOrdered()).toBeNull()
    expect(e.doc.getTextRange(0, 2, 0, 20)).toBe('A한🙂D')
    const undo = captureHwpUndo(peer, ops[0]!)
    applyHwpOp(peer, ops[0]!)
    expect(peer.getCollabFingerprint()).toBe(e.doc.getCollabFingerprint())
    const front = { section: 0, paragraph: 2, path: [], offset: 0 }
    const later: HwpOp = { kind: 'splice', from: front, to: front, text: '동료' }
    applyHwpOp(peer, later)
    const inverse = undo([later])
    expect(inverse?.length).toBeGreaterThan(0)
    for (const op of inverse!) applyHwpOp(peer, op)
    expect(peer.getTextRange(0, 2, 0, 20)).toBe('동료ABCD')
    expect(e.doc.getClipboardText()).toBe('가🙂나')
    e.select(null, at(4))
    e.paste(plain('!'))
    expect(e.insert).toHaveBeenCalledWith('!')
    expect(e.doc.getTextRange(0, 2, 0, 20)).toBe('A한🙂D!')
    expect(e.toast).not.toHaveBeenCalled()
  } finally {
    releaseHwpUndo(peer)
    peer.free()
  }
})

it('uses the marked native text and its styles, never the receiver clipboard or a document snapshot as an inverse', () => {
  const e = editor(),
    peer = e.doc.forkCollabDocument()
  try {
    peer.copySelection(0, 2, 0, 2, 1)
    e.paste(marked('가🙂나'))
    expect(e.submit).toHaveBeenCalledTimes(1)
    const ops = e.submit.mock.calls[0]![0]
    expect(ops).toHaveLength(2)
    applyHwpOp(peer, ops[0]!)
    expect(ops[1]).toMatchObject({
      kind: 'command',
      action: 'pasteText',
      fragment: { text: '가🙂나' },
    })
    expect(() => captureHwpUndo(peer, ops[1]!)).toThrow(
      expect.objectContaining({ code: 'unsupported' }),
    )
    applyHwpOp(peer, ops[1]!)
    expect(peer.getTextRange(0, 2, 0, 20)).toBe('A가🙂나D')
    expect(peer.getCollabFingerprint()).toBe(e.doc.getCollabFingerprint())
    expect(JSON.parse(peer.getCharPropertiesAt(0, 2, 2)).italic).toBe(true)
    expect(peer.getClipboardText()).toBe('A')
    expect(e.doc.getClipboardText()).toBe('가🙂나')
    expect(e.ih.cursor.getPosition()).toEqual(at(4))
  } finally {
    releaseHwpUndo(peer)
    peer.free()
  }
})

it('publishes none of a replacement if the admitted native paste fails after deleting; leaves notice to the transaction owner', () => {
  const e = editor(),
    before = e.doc.getCollabFingerprint()
  vi.spyOn(e.doc, 'pasteCollabClipboardText').mockImplementation(() => {
    throw new Error('native paste failed')
  })
  expect(() => e.paste(marked('가🙂나'))).toThrow('native paste failed')
  expect(e.submit).not.toHaveBeenCalled()
  expect(e.toast).not.toHaveBeenCalled()
  expect(e.doc.getCollabFingerprint()).toBe(before)
  expect(e.ih.cursor.getSelectionOrdered()).toEqual({ start: at(1), end: at(3) })
  expect(e.doc.getClipboardText()).toBe('가🙂나')
})

it('does not convert a native-only mobile paste or drag deletion into typed input; allows ordinary input and unbinds', () => {
  const field = document.createElement('textarea'),
    reject = vi.fn(),
    typed = vi.fn()
  const dispose = guardCollabTransferInput(field, reject)
  field.addEventListener('input', typed)
  const input = (type: string, inputType: string, cancelable = true) =>
    field.dispatchEvent(new InputEvent(type, { inputType, data: '가', cancelable }))
  expect(input('beforeinput', 'insertFromPaste')).toBe(false)
  expect(input('beforeinput', 'deleteByDrag')).toBe(false)
  input('beforeinput', 'insertFromPasteAsQuotation', false)
  field.value = 'unclassified'
  input('input', 'insertFromPasteAsQuotation', false)
  expect(field.value).toBe('')
  expect(typed).not.toHaveBeenCalled()
  expect(reject).toHaveBeenCalledTimes(3)
  input('input', 'insertText')
  dispose()
  input('input', 'insertFromDrop')
  expect(typed).toHaveBeenCalledTimes(2)
})

// Exercise the event/method bodies the pinned Studio actually installs, not a rewritten listener.
const tag = JSON.parse(
  readFileSync(join(__dirname, '../../../vendor/rhwp-core/package.json'), 'utf8'),
).rhwp.tag
const src = join(__dirname, '../node_modules/.cache/rhwp-studio', tag, 'rhwp-studio/src')
function entry(file: string, patch: string, name: string, bindings: Record<string, unknown>) {
  const code = applyPatch(patch, readFileSync(join(src, file), 'utf8'))
  const tree = ts.createSourceFile(file, code, ts.ScriptTarget.Latest, true)
  let fn: ts.MethodDeclaration | ts.FunctionDeclaration | undefined
  function visit(n: ts.Node) {
    if (
      (ts.isMethodDeclaration(n) || ts.isFunctionDeclaration(n)) &&
      n.name?.getText(tree) === name
    )
      fn = n
    if (!fn) ts.forEachChild(n, visit)
  }
  visit(tree)
  if (!fn?.body) throw new Error(`Pinned Studio has no ${name} input`)
  const args = fn.parameters
    .map((p) => p.name.getText(tree))
    .filter((s) => s !== 'this')
    .join(',')
  const js = ts.transpileModule(`function run(${args}) ${fn.body.getText(tree)}`, {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText
  return new Function(...Object.keys(bindings), `${js}\nreturn run`)(...Object.values(bindings))
}

it.skipIf(!existsSync(src))(
  'rejects the actual clipboard event before async RTF import or releasing an object; a later selection is not reused',
  async () => {
    const e = editor(),
      file = 'engine/input-handler-keyboard.ts',
      patch = 'studio-collab-paste'
    const { promise: pending, resolve: release } = Promise.withResolvers<string>()
    const inline = vi.fn(() => pending),
      pasteHtml = vi.fn(),
      html = '<img src="file:///clip.png">'
    const bindings = {
      studioCollab: e.collab,
      pasteStudioCollab,
      collabPasteKey,
      collabPasteNotice,
      showToast: e.toast,
      getLocale: () => 'en',
      hasCurrentRhwpClipboardMarker: current,
      pastePlainText: e.insert,
      extractHwpJsonModel: () => null,
      needsRtfImageInlining: () => true,
      inlineOfficeClipboardImages: inline,
      pasteExternalHtml: pasteHtml,
    }
    const ev = {
      preventDefault: vi.fn(),
      clipboardData: transfer({
        'text/html': html,
        'text/rtf': '{\\rtf1}',
        'text/plain': 'fallback',
      }),
    }
    const key = entry(file, patch, 'onKeyDown', bindings)
    const object = vi.spyOn(e.ih.cursor, 'isInPictureObjectSelection').mockReturnValue(true)
    for (const combo of [
      { ctrlKey: true, code: 'KeyV', key: 'ㅍ' },
      { metaKey: true, key: 'v' },
      { shiftKey: true, key: 'Insert' },
    ]) {
      const event = { ...combo, preventDefault: vi.fn() }
      key.call(e.ih, event)
      expect(event.preventDefault).not.toHaveBeenCalled()
    }
    const paste = entry(file, patch, 'onPaste', bindings)
    paste.call(e.ih, ev)
    expect(object).toHaveBeenCalledTimes(1)
    object.mockReturnValue(false)
    paste.call(e.ih, ev)
    e.select(at(0, 1), at(1, 1))
    release('<img src="data:image/png;base64,AQ==">')
    await pending
    expect(inline).not.toHaveBeenCalled()
    expect(pasteHtml).not.toHaveBeenCalled()
    expect(e.submit).not.toHaveBeenCalled()
    expect(e.ih.cursor.getSelectionOrdered()).toEqual({ start: at(0, 1), end: at(1, 1) })
    expect(e.toast).toHaveBeenCalledTimes(2)
    const solo = entry(file, patch, 'onPaste', { ...bindings, studioCollab: null })
    solo.call(
      {
        ...e.ih,
        cursor: { ...e.ih.cursor, isInHeaderFooter: () => false },
        wasm: { hasInternalClipboard: () => false },
      },
      ev,
    )
    await pending
    expect(inline).toHaveBeenCalledOnce()
    expect(pasteHtml).toHaveBeenCalledOnce()
  },
)

it.skipIf(!existsSync(src))(
  'refuses an image drop before the asynchronous prompt and refuses the snapshot undo before moving history',
  () => {
    const toast = vi.fn(),
      dialog = vi.fn(),
      arrayBuffer = vi.fn(),
      undo = vi.fn()
    const bindings = {
      studioCollab: {},
      collabPasteNotice,
      showToast: toast,
      getLocale: () => 'ko',
    }
    const page = document.implementation.createHTMLDocument()
    page.body.innerHTML = '<input id="file-input"><div id="scroll-container"></div>'
    const setup = entry('main.ts', 'studio-collab-paste-drop', 'setupFileInput', {
      ...bindings,
      document: page,
      documentFileInputAccept: () => '.hwp,.hwpx',
      isSupportedDocumentFileName: () => false,
      wasm: { hasPdfImportCapability: () => false },
      chromeMode: 'embed',
      showDropConfirmDialog: dialog,
    })
    setup()
    const drop = new Event('drop', { bubbles: true, cancelable: true })
    Object.defineProperty(drop, 'dataTransfer', {
      value: { files: [{ name: 'clip.png', arrayBuffer }] },
    })
    expect(page.getElementById('scroll-container')!.dispatchEvent(drop)).toBe(false)
    expect(dialog).not.toHaveBeenCalled()
    expect(arrayBuffer).not.toHaveBeenCalled()
    expect(toast).toHaveBeenLastCalledWith({ message: expect.stringContaining('위치를 지정') })
    const handleUndo = entry(
      'engine/input-handler.ts',
      'studio-collab-paste-input',
      'handleUndo',
      bindings,
    )
    handleUndo.call({
      history: { peekUndoTop: () => ({ type: 'snapshot:collabPasteText' }), undo },
    })
    expect(undo).not.toHaveBeenCalled()
    expect(toast).toHaveBeenLastCalledWith({ message: expect.stringContaining('문서 전체 복원') })
  },
)
