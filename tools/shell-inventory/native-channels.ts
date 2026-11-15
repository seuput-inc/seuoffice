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

export const NATIVE_CHANNEL_CONTRACTS: readonly NativeChannelContract[] = [
  {
    name: 'docs:owned-print-pdf-buffer',
    kinds: ['invoke'],
    batch: 'D2',
    surfaces: ['docs'],
    handler: 'crates/shell-ipc/d2/src/docs/export.rs',
    reason: 'Closing export resolves its existing Docs::Close.saves request scope; ordinary docs:print-pdf-buffer remains the original M contract.',
    arguments: ['widthTwips', 'heightTwips', 'scale?', 'ownedSaveRequestId'],
    requiredScenarios: ['normal-completion', 'missing-token', 'foreign-token', 'owner-loss', 'effect-failure', 'unauthorized-surface'],
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
    reason: 'Native recovery consumes the exact captured seed and returns its path and correlation token together.',
    arguments: [],
    requiredScenarios: ['ordinary-empty', 'captured-consume', 'duplicate-consume', 'foreign-renderer', 'preparation-loss', 'renderer-apply', 'unauthorized-surface'],
    synthetic: {
      suite: 'crates/shell-ipc/d2/src/pdf/editor/recovery_open/tests.rs',
      sourceCases: [
        {
          name: 'captured_recovery_completes_before_navigation_and_revocation_blocks_seed',
          scenarios: ['ordinary-empty', 'captured-consume', 'duplicate-consume', 'foreign-renderer', 'preparation-loss'],
        },
        { name: 'recovery_routes_reject_foreign_surface_in_the_real_dispatcher', scenarios: ['unauthorized-surface'] },
      ],
    },
  },
  {
    name: 'pdf:recovery-initialized',
    kinds: ['invoke'],
    batch: 'D2',
    surfaces: ['pdf'],
    handler: 'crates/shell-ipc/d2/src/pdf/editor/open.rs',
    reason: 'The registered PDF renderer settles its exact recovery receipt only after applying its document.',
    arguments: ['token', 'ok', 'error?'],
    requiredScenarios: ['successful-ack', 'missing-token', 'foreign-token', 'foreign-renderer', 'duplicate-ack', 'parse-failure', 'revision-loss', 'renderer-apply', 'unauthorized-surface'],
    synthetic: {
      suite: 'crates/shell-ipc/d2/src/pdf/editor/recovery_open/tests.rs',
      sourceCases: [
        {
          name: 'captured_recovery_completes_before_navigation_and_revocation_blocks_seed',
          scenarios: ['successful-ack', 'missing-token', 'foreign-token', 'foreign-renderer', 'duplicate-ack', 'parse-failure', 'revision-loss'],
        },
        { name: 'recovery_routes_reject_foreign_surface_in_the_real_dispatcher', scenarios: ['unauthorized-surface'] },
      ],
    },
  },
]
