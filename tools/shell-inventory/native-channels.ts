/** Native-only contracts have no Electron oracle. This is not a parity waiver. */
export interface NativeChannelContract {
  name: string
  kinds: readonly ('invoke' | 'send' | 'sendSync' | 'event' | 'port')[]
  batch: string
  surfaces: readonly string[]
  handler: string
  reason: string
  arguments: readonly string[]
  requiredScenarios: readonly string[]
  synthetic: {
    suite: string
    sourceCases: readonly { name: string; scenarios: readonly string[] }[]
  }
}

/**
 * A renderer's acknowledgement of a recovery copy or a saved input the native
 * shell handed it. The Electron main process registers no handler for any of
 * them (the renderer's call answers "No handler registered" there): only the
 * Rust shell waits for the acknowledgement, so no Electron case can compare it.
 */
function rendererAck(
  name: string,
  batch: 'D1' | 'D2',
  handler: string,
  what: string,
  args: readonly string[],
  suite: string,
  sourceCases: NativeChannelContract['synthetic']['sourceCases'],
): NativeChannelContract {
  return {
    name,
    kinds: ['invoke'],
    batch,
    surfaces: [name.split(':')[0]!],
    handler,
    reason: `The renderer acknowledges ${what} the native shell handed it; the native wait settles on that exact acknowledgement. The Electron main process registers no handler for this channel.`,
    arguments: args,
    requiredScenarios: [
      'successful-ack',
      'failed-apply',
      'missing-receipt',
      'foreign-receipt',
      'foreign-renderer',
      'duplicate-ack',
      'unauthorized-surface',
    ],
    synthetic: { suite, sourceCases },
  }
}

const RECOVERY_COPY = 'its exact recovery receipt after applying the recovery copy'
const RECOVERY_APPLIED = 'the recovery copy it applied'
const SAVED_INPUT = 'the saved input it applied'

export const NATIVE_CHANNEL_CONTRACTS: readonly NativeChannelContract[] = [
  {
    name: 'docs:owned-print-pdf-buffer',
    kinds: ['invoke'],
    batch: 'D2',
    surfaces: ['docs'],
    handler: 'crates/shell-ipc/d2/src/docs/export.rs',
    reason:
      'Closing export resolves its existing Docs::Close.saves request scope; ordinary docs:print-pdf-buffer remains the original M contract.',
    arguments: ['widthTwips', 'heightTwips', 'scale?', 'ownedSaveRequestId'],
    requiredScenarios: [
      'normal-completion',
      'missing-token',
      'foreign-token',
      'owner-loss',
      'effect-failure',
      'unauthorized-surface',
    ],
    synthetic: {
      suite: 'crates/shell-ipc/d2/src/docs/export/owned/tests.rs',
      sourceCases: [
        {
          name: 'owned_print_requires_its_pending_token_and_exports_reject_foreign_tokens_before_effects',
          scenarios: ['missing-token', 'foreign-token', 'unauthorized-surface'],
        },
        {
          name: 'owned_print_consumes_real_save_scope_and_waits_for_exact_ack_or_owner_loss',
          scenarios: ['normal-completion', 'owner-loss', 'effect-failure'],
        },
      ],
    },
  },
  {
    name: 'pdf:consume-recovery-pending',
    kinds: ['invoke'],
    batch: 'D2',
    surfaces: ['pdf'],
    handler: 'crates/shell-ipc/d2/src/pdf/editor/open.rs',
    reason:
      'Native recovery consumes the exact captured seed and returns its path and correlation token together.',
    arguments: [],
    requiredScenarios: [
      'ordinary-empty',
      'captured-consume',
      'duplicate-consume',
      'foreign-renderer',
      'preparation-loss',
      'renderer-apply',
      'unauthorized-surface',
    ],
    synthetic: {
      suite: 'crates/shell-ipc/d2/src/pdf/editor/recovery_open/tests.rs',
      sourceCases: [
        {
          name: 'captured_recovery_completes_before_navigation_and_revocation_blocks_seed',
          scenarios: [
            'ordinary-empty',
            'captured-consume',
            'duplicate-consume',
            'foreign-renderer',
            'preparation-loss',
          ],
        },
        {
          name: 'recovery_routes_reject_foreign_surface_in_the_real_dispatcher',
          scenarios: ['unauthorized-surface'],
        },
      ],
    },
  },
  {
    name: 'pdf:recovery-initialized',
    kinds: ['invoke'],
    batch: 'D2',
    surfaces: ['pdf'],
    handler: 'crates/shell-ipc/d2/src/pdf/editor/open.rs',
    reason:
      'The registered PDF renderer settles its exact recovery receipt only after applying its document.',
    arguments: ['token', 'ok', 'error?'],
    requiredScenarios: [
      'successful-ack',
      'missing-token',
      'foreign-token',
      'foreign-renderer',
      'duplicate-ack',
      'parse-failure',
      'revision-loss',
      'renderer-apply',
      'unauthorized-surface',
    ],
    synthetic: {
      suite: 'crates/shell-ipc/d2/src/pdf/editor/recovery_open/tests.rs',
      sourceCases: [
        {
          name: 'captured_recovery_completes_before_navigation_and_revocation_blocks_seed',
          scenarios: [
            'successful-ack',
            'missing-token',
            'foreign-token',
            'foreign-renderer',
            'duplicate-ack',
            'parse-failure',
            'revision-loss',
          ],
        },
        {
          name: 'recovery_routes_reject_foreign_surface_in_the_real_dispatcher',
          scenarios: ['unauthorized-surface'],
        },
      ],
    },
  },
  {
    name: 'workbook:saved-open-result',
    kinds: ['invoke'],
    batch: 'D1',
    surfaces: ['sheets'],
    handler: 'crates/shell-ipc/d1/src/sheets/mod.rs',
    reason:
      'A saved workbook opens only after the renderer reports its first range loaded; the report settles the exact saved-load barrier and drops the session on failure. The Electron shell opens the file in main and never waits for the renderer, so it has no equivalent channel.',
    arguments: ['id', 'ok', 'error?'],
    requiredScenarios: [
      'missing-identity',
      'missing-result',
      'inconsistent-error',
      'missing-native-authority',
      'closed-tab',
      'no-opening',
    ],
    synthetic: {
      suite: 'crates/shell-ipc/d1/src/sheets/tests.rs',
      sourceCases: [
        {
          name: 'saved_open_result_rejects_acknowledgements_without_identity_result_or_a_consistent_error',
          scenarios: ['missing-identity', 'missing-result', 'inconsistent-error', 'no-opening'],
        },
        {
          name: 'saved_open_result_needs_native_authority_and_a_live_tab',
          scenarios: ['missing-native-authority', 'closed-tab'],
        },
      ],
    },
  },
  rendererAck(
    'diagram:recovery-initialized',
    'D1',
    'crates/shell-ipc/d1/src/diagram/mod.rs',
    RECOVERY_COPY,
    ['token', 'ok', 'error?'],
    'crates/shell-ipc/d1/src/diagram/tests.rs',
    [
      {
        name: 'recovery_file_waits_for_exact_renderer_and_initialized_ack',
        scenarios: ['successful-ack', 'foreign-renderer'],
      },
    ],
  ),
  rendererAck(
    'mindmap:recovery-initialized',
    'D1',
    'crates/shell-ipc/d1/src/mindmap/handlers.rs',
    RECOVERY_COPY,
    ['token', 'ok', 'error?'],
    'crates/shell-ipc/d1/src/mindmap/tests.rs',
    [],
  ),
  rendererAck(
    'slides:recovery-initialized',
    'D1',
    'crates/shell-ipc/d1/src/slides/mod.rs',
    RECOVERY_COPY,
    ['token', 'ok', 'error?'],
    'crates/shell-ipc/d1/src/slides/tests.rs',
    [{ name: 'selected_recovery_decision_opens_the_selected_deck', scenarios: ['successful-ack'] }],
  ),
  rendererAck(
    'docs:recovery-initialized',
    'D2',
    'crates/shell-ipc/d2/src/docs/open.rs',
    RECOVERY_COPY,
    ['receipt', 'ok', 'error?'],
    'crates/shell-ipc/d2/src/docs/tests.rs',
    [
      {
        name: 'blank_recovery_waits_for_authenticated_renderer_initialization',
        scenarios: ['successful-ack', 'foreign-renderer'],
      },
      {
        name: 'captured_recovery_waits_for_real_copy_preparation_before_navigation',
        scenarios: ['successful-ack'],
      },
    ],
  ),
  rendererAck(
    'epub:recovery-initialized',
    'D2',
    'crates/shell-ipc/d2/src/epub/mod.rs',
    RECOVERY_COPY,
    ['receipt', 'ok', 'error?'],
    'crates/shell-ipc/d2/src/epub/tests.rs',
    [
      {
        name: 'native_recovery_requires_the_registered_renderer_and_real_book_before_ack',
        scenarios: ['successful-ack', 'foreign-renderer'],
      },
      {
        name: 'native_recovery_refusal_settles_failure_without_a_book',
        scenarios: ['failed-apply'],
      },
    ],
  ),
  rendererAck(
    'hwp:recovery-initialized',
    'D2',
    'crates/shell-ipc/d2/src/hwp/editor.rs',
    RECOVERY_COPY,
    ['token', 'result'],
    'crates/shell-ipc/d2/src/hwp/editor/tests.rs',
    [
      {
        name: 'recovery_receipt_waits_for_the_matching_initialized_document',
        scenarios: ['successful-ack', 'foreign-receipt'],
      },
    ],
  ),
  rendererAck(
    'html:recovery-applied',
    'D2',
    'crates/shell-ipc/d2/src/html/mod.rs',
    RECOVERY_APPLIED,
    ['result'],
    'crates/shell-ipc/d2/src/html/tests.rs',
    [
      {
        name: 'recovery_restore_reads_before_load_and_delivers_only_to_reserved_renderer',
        scenarios: ['successful-ack', 'foreign-renderer'],
      },
    ],
  ),
  rendererAck(
    'html:saved-input-applied',
    'D2',
    'crates/shell-ipc/d2/src/html/mod.rs',
    SAVED_INPUT,
    ['result'],
    'crates/shell-ipc/d2/src/html/tests.rs',
    [],
  ),
  rendererAck(
    'markdown:recovery-applied',
    'D2',
    'crates/shell-ipc/d2/src/markdown/mod.rs',
    RECOVERY_APPLIED,
    ['result'],
    'crates/shell-ipc/d2/src/markdown/tests.rs',
    [],
  ),
  rendererAck(
    'markdown:saved-input-applied',
    'D2',
    'crates/shell-ipc/d2/src/markdown/mod.rs',
    SAVED_INPUT,
    ['result'],
    'crates/shell-ipc/d2/src/markdown/tests.rs',
    [],
  ),
]
