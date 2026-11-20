/**
 * Required scenarios of the native-only contracts (native-channels.ts) that the real app cannot
 * reach, so their proof is synthetic only (hub decision hub-g4-contract-decision-20261120).
 *
 * The IPC comparison (tools/ipc-diff/native-contracts.ts) asks native-host evidence for every
 * required scenario except these, and synthetic evidence for all of them: a waived scenario
 * whose synthetic receipt is missing stays red. A native-host receipt that names a waived
 * scenario is an error there, because a spec that drives it makes the waiver wrong.
 *
 * The native-host specs (crates/shell/e2e/native-host/specs.mjs) name these as `UNDRIVEN`, and
 * the checker's test fails when the specs and this list together stop matching the contracts.
 * The list is closed: a new entry needs a new decision.
 */

const SAVED_INPUT = { 'failed-apply': 'no text makes the editor fail to apply a file' }

export const SYNTHETIC_ONLY = {
  'docs:owned-print-pdf-buffer': {
    'owner-loss': 'needs the tab closed while the copy is being printed',
    'effect-failure': 'needs the print of the page itself to fail',
  },
  'slides:recovery-initialized': {
    'failed-apply':
      'the shell parses the presentation itself: a copy that does not parse fails the open ' +
      'before any page is handed a receipt',
  },
  'pdf:consume-recovery-pending': {
    'preparation-loss': 'needs the restored tab revoked while its document is being prepared',
  },
  'pdf:recovery-initialized': {
    'revision-loss':
      'needs the document revision replaced between the hand-over and the acknowledgement',
  },
  'workbook:saved-open-result': {
    'missing-native-authority':
      'a page of the real app cannot send a call without its native context',
    'closed-tab': 'a closed tab has no page left to send the report from',
  },
  'html:recovery-applied': { 'failed-apply': 'no text makes the editor fail to apply a copy' },
  'html:saved-input-applied': SAVED_INPUT,
  'markdown:recovery-applied': { 'failed-apply': 'no text makes the editor fail to apply a copy' },
  'markdown:saved-input-applied': SAVED_INPUT,
}
