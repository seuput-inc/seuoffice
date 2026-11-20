/**
 * Group `w/doc-picker`: the "Attach from documents" window
 * (`apps/shell/src/main/doc-picker/doc-picker-window.ts`) and the editors'
 * `doc:*` channels that open it and build the `@` list. The window is shown
 * only when its page invokes `doc-picker:ready` with the window's token. An
 * invoke that waits for the person is either a deferred call (`defer`, settled
 * by an `await` step), or comes from an editor that asked before the first call
 * (`hosts.w.docPicker.asked`, answer logged when it comes), or ends because the
 * person closes the window as soon as it is created (`userCloses`);
 * support/doc-picker.ts says what a case may describe.
 */
import { createDocList } from '../../../../apps/shell/src/main/doc-picker/doc-list.ts'
import { specOf, standIn } from '../../support/doc-picker.ts'
import type { Case, Group, View } from '../../types.ts'

const MODULE = 'apps/shell/src/main/doc-picker/doc-picker-window.ts'
type DocPickerWindow =
  typeof import('../../../../apps/shell/src/main/doc-picker/doc-picker-window.ts')

const EDITOR: View = { view: 1, window: 1, surface: 'Docs' }
const PICKER: View = { view: 2, window: 2, surface: 'ShellDocPicker' }
const SHEETS: View = { view: 3, window: 3, surface: 'Sheets' }
const OTHER_PICKER: View = { view: 4, window: 4, surface: 'ShellDocPicker' }
const SHELL: View = { view: 5, window: 5, surface: 'Shell' }
const VIEWS = [EDITOR, PICKER, SHEETS, OTHER_PICKER, SHELL]

const call = (channel: string, view: number, ...args: unknown[]) => ({ channel, view, args })
const init = (view = 2) => call('doc-picker:get-init', view)
const list = (view = 2) => call('doc-picker:list', view)
const choose = (ids: unknown, view = 2) => call('doc-picker:choose', view, ids)
const cancel = (view = 2) => call('doc-picker:cancel', view)
const ready = (token: unknown, view = 2) => call('doc-picker:ready', view, token)
const available = (view = 1) => call('doc:attach-available', view)
const pick = (view = 1) => call('doc:pick-attachments', view)
const attachList = (view = 1) => call('doc:attach-list', view)

const hosts = (docPicker: Record<string, unknown>) => ({ w: { docPicker } })

const OPEN = [
  { id: 't1', path: '/w/a.docx', title: 'a.docx', dirty: false },
  { id: 't2', path: '/w/b.xlsx', title: 'b.xlsx', dirty: true },
  { id: 't3', title: 'Untitled 1', dirty: false },
  { id: 't4', path: '/w/c.pdf', title: 'c.pdf', dirty: false },
  { id: 't5', path: '/w/notes.md', title: 'notes.md', dirty: false },
]
const RECENT = [
  { path: '/r/one.txt', title: 'one.txt', missing: false },
  { path: '/r/gone.md', title: 'gone.md', missing: true },
  { path: '/w/a.docx', title: 'a.docx', missing: false },
  { path: '/r/huge.txt', title: 'huge.txt', missing: false },
  { path: '/r/app.exe', title: 'app.exe', missing: false },
  { path: '/r/flow.drawio.png', title: 'flow', missing: false },
  { path: '/r/deleted.csv', title: 'deleted.csv', missing: false },
]
const SIZES = {
  '/w/a.docx': 100,
  '/w/b.xlsx': 200,
  '/w/c.pdf': 300,
  '/w/notes.md': 40,
  '/r/one.txt': 10,
  '/r/huge.txt': 60 * 1024 * 1024,
  '/r/app.exe': 1,
  '/r/flow.drawio.png': 6 * 1024 * 1024,
}
const ASKER = { tabId: 't1', path: '/w/a.docx', paneTabId: 't4' }
const PROJECT = [
  { path: '/p/plan.md', title: 'plan' },
  { path: '/w/notes.md', title: 'notes again' },
  { path: '/p/table.csv', title: 'table' },
]
const full = {
  open: OPEN,
  recent: RECENT,
  sizes: { ...SIZES, '/p/plan.md': 5, '/p/table.csv': 6 },
}
const asked = { parent: 1, asker: ASKER }

const many = Array.from({ length: 25 }, (_, i) => `/m/doc-${String(i).padStart(2, '0')}.txt`)
const manySpec = {
  recent: many.map((path) => ({ path, title: path.slice(3), missing: false })),
  sizes: Object.fromEntries(many.map((path) => [path, 1])),
  asked,
}

const cases: Case[] = [
  {
    note: 'the availability probe answers for every editor',
    views: VIEWS,
    calls: [available(), available(3), available(5)],
  },
  {
    note: 'the person closes the window: the pick is cancelled',
    views: VIEWS,
    hosts: hosts({ userCloses: true, askers: { '1': ASKER } }),
    calls: [pick(), init(), pick(3)],
  },
  {
    note: 'English and a dark theme, another picker view',
    views: VIEWS,
    hosts: hosts({ userCloses: true, lang: 'en', theme: 'dark', view: 4 }),
    calls: [pick(3)],
  },
  {
    note: 'a second asker finds the window busy and brings it forward',
    views: VIEWS,
    hosts: hosts({ asked }),
    calls: [pick(3), pick(1), cancel()],
  },
  {
    note: 'the picker window reads its init',
    views: VIEWS,
    hosts: hosts({ asked, lang: 'en', theme: 'dark' }),
    calls: [init(), init(4), init(5)],
  },
  {
    note: 'no window: the init is null, a list and a choice are unavailable',
    views: VIEWS,
    calls: [init(), list(), choose(['/w/a.docx']), cancel()],
  },
  {
    note: 'the list is built for the editor that asked',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [list(), list(4), list(1)],
  },
  {
    note: 'an asker that is not an editor the shell knows leaves nothing out',
    views: VIEWS,
    hosts: hosts({ ...full, asked: { parent: 1, asker: {} } }),
    calls: [list()],
  },
  {
    note: 'no open documents and no recent files',
    views: VIEWS,
    hosts: hosts({ asked }),
    calls: [list()],
  },
  {
    note: 'an asker without a pane and a project does not change the picker list',
    views: VIEWS,
    hosts: hosts({
      ...full,
      project: PROJECT,
      asked: { parent: 1, asker: { tabId: 't2', path: '/w/b.xlsx' } },
    }),
    calls: [list()],
  },
  {
    note: 'choose after list hands the ticked paths back and closes the window',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [list(), choose(['/w/b.xlsx', '/r/one.txt', '/w/notes.md']), list(), init()],
  },
  {
    note: 'only ids the list held as ok survive, once each',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [
      list(),
      choose([
        '/w/notes.md',
        '/w/b.xlsx',
        'unsaved:t3',
        '/r/gone.md',
        '/etc/passwd',
        '/w/notes.md',
        7,
        null,
        { id: 'x' },
        '/r/one.txt',
        'x'.repeat(5000),
      ]),
    ],
  },
  {
    note: 'a choice before the list was read is a cancel',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [choose(['/w/notes.md'])],
  },
  {
    note: 'a choice of nothing valid is a cancel',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [list(), choose(['/r/gone.md', 'unsaved:t3'])],
  },
  {
    note: 'not an array, no argument',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [
      list(),
      choose('/w/notes.md'),
      choose(null),
      call('doc-picker:choose', 2),
      choose({ 0: '/w/notes.md' }),
    ],
  },
  {
    note: 'an empty choice is a cancel',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [list(), choose([])],
  },
  {
    note: 'twenty documents at most',
    views: VIEWS,
    hosts: hosts(manySpec),
    calls: [list(), choose(many)],
  },
  {
    note: 'a choice from a view that is not the window is unavailable',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [list(), choose(['/w/notes.md'], 4), choose(['/w/notes.md'], 4), cancel(4), list(4)],
  },
  {
    note: 'cancel closes the window and cancels the pick',
    views: VIEWS,
    hosts: hosts({ ...full, asked }),
    calls: [cancel(4), cancel(), list()],
  },
  {
    note: 'a second list replaces the first for the next choice',
    views: VIEWS,
    hosts: hosts({
      open: [{ id: 't1', path: '/w/a.docx', title: 'a.docx', dirty: false }],
      sizes: { '/w/a.docx': 10 },
      asked: { parent: 1, asker: {} },
    }),
    calls: [list(), choose(['/w/a.docx'])],
  },
  {
    note: 'the @ list of an editor: its own document out, missing files out',
    views: VIEWS,
    hosts: hosts({ ...full, askers: { '1': ASKER } }),
    calls: [attachList(), attachList(3)],
  },
  {
    note: 'the @ list adds the files of the editor project, newest first',
    views: VIEWS,
    hosts: hosts({ ...full, project: PROJECT, askers: { '1': ASKER } }),
    calls: [attachList(), attachList(3)],
  },
  {
    note: 'the project list throws: the list goes on without it',
    views: VIEWS,
    hosts: hosts({ ...full, project: { throws: true }, askers: { '1': ASKER } }),
    calls: [attachList()],
  },
  {
    note: 'an empty @ list',
    views: VIEWS,
    calls: [attachList(), attachList(5)],
  },
  {
    note: 'the picker page says it is ready: the window is shown, once',
    views: VIEWS,
    hosts: hosts({ asked }),
    calls: [init(), ready('1'), ready('1'), cancel()],
  },
  {
    note: 'a ready from another view, with another token or none shows nothing',
    views: VIEWS,
    hosts: hosts({ asked }),
    calls: [
      ready('1', 4),
      ready('0'),
      ready('2'),
      ready(1),
      call('doc-picker:ready', 2),
      ready('1', 1),
      ready('1', 5),
      cancel(),
    ],
  },
  {
    note: 'no window: a ready shows nothing',
    views: VIEWS,
    calls: [ready('1'), ready('0')],
  },
  {
    note: 'a window opened after another closed has a token of its own',
    views: VIEWS,
    hosts: hosts({ asked }),
    calls: [
      cancel(),
      { ...pick(3), defer: true },
      init(),
      ready('1'),
      ready('2'),
      cancel(),
      { channel: '', args: [], await: 1 },
    ],
  },
]

export const group: Group = {
  batch: 'W',
  async load(cx) {
    const { deps } = standIn(cx)
    const mod = await cx.fresh<DocPickerWindow>(MODULE)
    const picker = mod.createDocPicker({
      list: createDocList(deps.listDeps),
      askerFor: deps.askerFor,
      lang: deps.lang,
      theme: deps.theme,
    })
    const asked = specOf(cx).asked
    if (asked) {
      const parent = (cx.electron.BrowserWindow as { fromId(id: number): never }).fromId(
        asked.parent,
      )
      void picker
        .open(parent, asked.asker)
        .then((answer) => cx.electron.log.push(['doc:pick-attachments settled', answer]))
      // a person who closes the window has done so before the first call
      for (let i = 0; i < 4; i++) await new Promise((r) => setImmediate(r))
    }
  },
  cases,
  single: {
    'doc:attach-available': 'it answers true to every editor',
  },
}
