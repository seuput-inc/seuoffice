//! Owned continuation for recovery launch. External work is represented by
//! explicit requests; advancing it never waits for a renderer or filesystem.
use super::{LaunchDeps, LaunchManager, RecoveryEditors, RecoveryShell, UnloadedTabSpec, tab_spec};
use crate::packages::electron_utils::recovery_store::{RecoveryIdentity, RecoveryLaunchDecision};
use crate::shell::launch_recovery::{
    LaunchOutcome, LaunchPlan, PlannedTab, RecoveryListItem, RecoveryListResult,
    resolve_launch_decisions,
};
use crate::shell::renderer_disposal::Failure;
use std::collections::HashMap;

#[derive(Debug)]
pub enum LaunchNeed {
    Prepare {
        user_data: String,
        paths: Vec<String>,
        reopened_by_installer: bool,
    },
    RecordDecisions(Vec<(RecoveryIdentity, RecoveryLaunchDecision)>),
    Hydrate(Vec<UnloadedTabSpec>),
    SessionChanged,
    CloseTab(String),
    AddUnloaded(UnloadedTabSpec),
    Reorder {
        id: String,
        slot: usize,
    },
    OpenHome,
    BeginSplit {
        a: String,
        b: String,
        orientation: String,
        ratio: f64,
        focus: String,
    },
    Activate(String),
    OpenPaths(Vec<String>),
    RestoreSplitOrHome,
    Settled,
}

pub enum LaunchReply {
    Prepared(Result<LaunchPlan, Failure>),
    Decisions(Vec<Failure>),
    Hydrated(Result<Vec<String>, Failure>),
    Added(Result<String, Failure>),
    Completed(Result<(), Failure>),
}

pub enum LaunchStep {
    Need(LaunchNeed, LaunchMachine),
    AwaitList(Vec<RecoveryListItem>, LaunchMachine),
    Done(Result<(), Failure>),
}

enum Phase {
    Prepare,
    Hydrate,
    SessionChanged,
    HomeForList,
    Decisions,
    Remove(String),
    AddReplacement {
        planned: PlannedTab,
        old: Option<String>,
        slot: Option<usize>,
    },
    ReorderReplacement {
        key: String,
        old: Option<String>,
        new: String,
    },
    CloseReplacement {
        key: String,
        new: String,
    },
    AddAppend(PlannedTab),
    Split,
    Activate,
    LayoutTail,
    PlainTail,
    FallbackHome,
    Settled,
    PlainSettled,
    AwaitList,
}

pub struct LaunchMachine {
    paths: Vec<String>,
    ids: HashMap<String, String>,
    plan: Option<LaunchPlan>,
    outcome: Option<LaunchOutcome>,
    cursor: usize,
    phase: Phase,
}

impl LaunchMachine {
    pub fn start(user_data: String, paths: Vec<String>, reopened_by_installer: bool) -> LaunchStep {
        let need = LaunchNeed::Prepare {
            user_data,
            paths: paths.clone(),
            reopened_by_installer,
        };
        LaunchStep::Need(
            need,
            Self {
                paths,
                ids: HashMap::new(),
                plan: None,
                outcome: None,
                cursor: 0,
                phase: Phase::Prepare,
            },
        )
    }

    fn need(mut self, phase: Phase, need: LaunchNeed) -> LaunchStep {
        self.phase = phase;
        LaunchStep::Need(need, self)
    }

    pub fn answer<E: RecoveryEditors, D: LaunchDeps>(
        mut self,
        result: RecoveryListResult,
        shell: &mut RecoveryShell,
        editors: &E,
        deps: &mut D,
    ) -> LaunchStep {
        if !matches!(&self.phase, Phase::AwaitList) {
            return LaunchStep::Done(Err(Failure(
                "recovery list is not awaiting an answer".into(),
            )));
        }
        let Some(plan) = self.plan.take() else {
            return LaunchStep::Done(Err(Failure("recovery plan is unavailable".into())));
        };
        let outcome = resolve_launch_decisions(&plan, Some(&result));
        if outcome.store_decisions.is_empty() {
            return self.layout(shell, editors, deps, outcome);
        }
        let decisions = outcome.store_decisions.clone();
        self.outcome = Some(outcome);
        self.need(Phase::Decisions, LaunchNeed::RecordDecisions(decisions))
    }

    pub fn resume<E: RecoveryEditors, D: LaunchDeps>(
        mut self,
        reply: LaunchReply,
        shell: &mut RecoveryShell,
        editors: &E,
        deps: &mut D,
    ) -> LaunchStep {
        let phase = std::mem::replace(&mut self.phase, Phase::Prepare);
        match (phase, reply) {
            (Phase::Prepare, LaunchReply::Prepared(prepared)) => match prepared {
                Err(error) => {
                    editors.warn(
                        "[recovery] the launch could not restore the previous session:",
                        &error.0,
                    );
                    self.plain(deps)
                }
                Ok(plan) if plan.plain_launch => self.plain(deps),
                Ok(plan) => {
                    let tabs = plan.tabs.iter().map(tab_spec).collect();
                    self.plan = Some(plan);
                    self.need(Phase::Hydrate, LaunchNeed::Hydrate(tabs))
                }
            },
            (Phase::Hydrate, LaunchReply::Hydrated(result)) => match result {
                Err(error) => {
                    editors.warn(
                        "[recovery] the launch could not restore the previous session:",
                        &error.0,
                    );
                    self.plain(deps)
                }
                Ok(ids) => {
                    let plan = self.plan.as_ref().expect("pending recovery plan");
                    for (tab, id) in plan.tabs.iter().zip(ids) {
                        self.ids.insert(tab.key.clone(), id.clone());
                        shell.note_planned(&id, tab);
                    }
                    self.need(Phase::SessionChanged, LaunchNeed::SessionChanged)
                }
            },
            (Phase::SessionChanged, LaunchReply::Completed(result)) => {
                if let Err(error) = result {
                    editors.warn(
                        "[recovery] the session record could not be updated:",
                        &error.0,
                    );
                }
                let plan = self.plan.as_ref().expect("pending recovery plan");
                if plan.items.is_empty() {
                    deps.gate_publish_none();
                    let outcome = resolve_launch_decisions(plan, None);
                    self.layout(shell, editors, deps, outcome)
                } else {
                    self.need(Phase::HomeForList, LaunchNeed::OpenHome)
                }
            }
            (Phase::HomeForList, LaunchReply::Completed(result)) => match result {
                Err(error) => LaunchStep::Done(Err(error)),
                Ok(()) => {
                    let items = self
                        .plan
                        .as_ref()
                        .expect("pending recovery plan")
                        .items
                        .clone();
                    self.phase = Phase::AwaitList;
                    LaunchStep::AwaitList(items, self)
                }
            },
            (Phase::Decisions, LaunchReply::Decisions(errors)) => {
                for error in errors {
                    editors.warn(
                        "[recovery] a launch decision could not be recorded:",
                        &error.0,
                    );
                }
                let outcome = self.outcome.take().expect("pending recovery outcome");
                self.layout(shell, editors, deps, outcome)
            }
            (Phase::Remove(key), LaunchReply::Completed(result)) => {
                if let Err(error) = result {
                    return self.layout_failed(editors, error);
                }
                self.ids.remove(&key);
                self.next_layout(shell, editors, deps)
            }
            (Phase::AddReplacement { planned, old, slot }, LaunchReply::Added(result)) => {
                match result {
                    Err(error) => self.layout_failed(editors, error),
                    Ok(new) => {
                        shell.note_planned(&new, &planned);
                        let key = planned.key;
                        if let Some(slot) = slot.filter(|slot| *slot > 0) {
                            let need = LaunchNeed::Reorder {
                                id: new.clone(),
                                slot,
                            };
                            self.need(Phase::ReorderReplacement { key, old, new }, need)
                        } else {
                            self.finish_replacement(shell, editors, deps, key, old, new)
                        }
                    }
                }
            }
            (Phase::ReorderReplacement { key, old, new }, LaunchReply::Completed(result)) => {
                if let Err(error) = result {
                    return self.layout_failed(editors, error);
                }
                self.finish_replacement(shell, editors, deps, key, old, new)
            }
            (Phase::CloseReplacement { key, new }, LaunchReply::Completed(result)) => {
                if let Err(error) = result {
                    return self.layout_failed(editors, error);
                }
                self.ids.insert(key, new);
                self.next_layout(shell, editors, deps)
            }
            (Phase::AddAppend(planned), LaunchReply::Added(result)) => match result {
                Err(error) => self.layout_failed(editors, error),
                Ok(id) => {
                    shell.note_planned(&id, &planned);
                    self.ids.insert(planned.key, id);
                    self.next_layout(shell, editors, deps)
                }
            },
            (Phase::Split, LaunchReply::Completed(result)) => {
                if let Err(error) = result {
                    return self.layout_failed(editors, error);
                }
                self.after_split(editors, deps)
            }
            (Phase::Activate, LaunchReply::Completed(result)) => {
                if let Err(error) = result {
                    return self.layout_failed(editors, error);
                }
                self.layout_tail(editors, deps)
            }
            (Phase::LayoutTail, LaunchReply::Completed(result)) => match result {
                Ok(()) => self.need(Phase::Settled, LaunchNeed::Settled),
                Err(error) => self.layout_failed(editors, error),
            },
            (Phase::PlainTail, LaunchReply::Completed(result)) => match result {
                Ok(()) => self.need(Phase::PlainSettled, LaunchNeed::Settled),
                Err(error) => LaunchStep::Done(Err(error)),
            },
            (Phase::FallbackHome, LaunchReply::Completed(result)) => {
                if let Err(error) = result {
                    editors.warn("[recovery] Home could not be shown:", &error.0);
                }
                self.need(Phase::Settled, LaunchNeed::Settled)
            }
            (Phase::Settled, LaunchReply::Completed(result)) => {
                if let Err(error) = result {
                    editors.warn(
                        "[recovery] the files that arrived during the launch could not open:",
                        &error.0,
                    );
                }
                LaunchStep::Done(Ok(()))
            }
            (Phase::PlainSettled, LaunchReply::Completed(result)) => LaunchStep::Done(result),
            _ => LaunchStep::Done(Err(Failure("unexpected recovery launch completion".into()))),
        }
    }

    fn plain<D: LaunchDeps>(self, deps: &mut D) -> LaunchStep {
        deps.gate_publish_none();
        if self.paths.is_empty() {
            self.need(Phase::PlainTail, LaunchNeed::RestoreSplitOrHome)
        } else {
            let paths = self.paths.clone();
            self.need(Phase::PlainTail, LaunchNeed::OpenPaths(paths))
        }
    }

    fn layout<E: RecoveryEditors, D: LaunchDeps>(
        mut self,
        shell: &mut RecoveryShell,
        editors: &E,
        deps: &mut D,
        outcome: LaunchOutcome,
    ) -> LaunchStep {
        self.outcome = Some(outcome);
        self.cursor = 0;
        self.next_layout(shell, editors, deps)
    }

    fn next_layout<E: RecoveryEditors, D: LaunchDeps>(
        mut self,
        _shell: &mut RecoveryShell,
        editors: &E,
        deps: &mut D,
    ) -> LaunchStep {
        loop {
            let outcome = self.outcome.as_ref().expect("pending recovery outcome");
            if let Some(key) = outcome.remove.get(self.cursor).cloned() {
                self.cursor += 1;
                if let Some(id) = self.ids.get(&key).filter(|id| !id.is_empty()).cloned() {
                    return self.need(Phase::Remove(key), LaunchNeed::CloseTab(id));
                }
                self.ids.remove(&key);
                continue;
            }
            let replace_index = self.cursor - outcome.remove.len();
            if let Some(planned) = outcome.replace.get(replace_index).cloned() {
                self.cursor += 1;
                let old = self.ids.get(&planned.key).cloned();
                let slot = match deps.manager().list() {
                    Ok(tabs) => tabs.iter().position(|tab| Some(&tab.id) == old.as_ref()),
                    Err(error) => return self.layout_failed(editors, error),
                };
                let spec = tab_spec(&planned);
                return self.need(
                    Phase::AddReplacement { planned, old, slot },
                    LaunchNeed::AddUnloaded(spec),
                );
            }
            let append_index = replace_index - outcome.replace.len();
            if let Some(planned) = outcome.append.get(append_index).cloned() {
                self.cursor += 1;
                let spec = tab_spec(&planned);
                return self.need(Phase::AddAppend(planned), LaunchNeed::AddUnloaded(spec));
            }
            let split = outcome.split.as_ref().and_then(|split| {
                Some((
                    split,
                    self.ids.get(&split.a)?.clone(),
                    self.ids.get(&split.b)?.clone(),
                ))
            });
            if let Some((split, a, b)) = split.filter(|(_, a, b)| !a.is_empty() && !b.is_empty()) {
                let focus = if outcome.front.as_deref() == Some(split.a.as_str()) {
                    "a"
                } else if outcome.front.as_deref() == Some(split.b.as_str()) {
                    "b"
                } else {
                    &split.focus
                };
                let need = LaunchNeed::BeginSplit {
                    a,
                    b,
                    orientation: split.orientation.clone(),
                    ratio: split.ratio,
                    focus: focus.into(),
                };
                return self.need(Phase::Split, need);
            }
            return self.after_split(editors, deps);
        }
    }

    fn finish_replacement<E: RecoveryEditors, D: LaunchDeps>(
        mut self,
        shell: &mut RecoveryShell,
        editors: &E,
        deps: &mut D,
        key: String,
        old: Option<String>,
        new: String,
    ) -> LaunchStep {
        if let Some(old) = old.filter(|id| !id.is_empty()) {
            self.need(
                Phase::CloseReplacement { key, new },
                LaunchNeed::CloseTab(old),
            )
        } else {
            self.ids.insert(key, new);
            self.next_layout(shell, editors, deps)
        }
    }

    fn after_split<E: RecoveryEditors, D: LaunchDeps>(
        self,
        editors: &E,
        deps: &mut D,
    ) -> LaunchStep {
        let split = match deps.manager().split_state() {
            Ok(split) => split,
            Err(error) => return self.layout_failed(editors, error),
        };
        let front = self
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.front.as_ref())
            .and_then(|key| self.ids.get(key))
            .filter(|id| !id.is_empty())
            .cloned();
        if !split && let Some(front) = front {
            return self.need(Phase::Activate, LaunchNeed::Activate(front));
        }
        self.layout_tail(editors, deps)
    }

    fn layout_tail<E: RecoveryEditors, D: LaunchDeps>(
        self,
        editors: &E,
        deps: &mut D,
    ) -> LaunchStep {
        let outcome = self.outcome.as_ref().expect("pending recovery outcome");
        if !outcome.launch_paths.is_empty() {
            let paths = outcome.launch_paths.clone();
            return self.need(Phase::LayoutTail, LaunchNeed::OpenPaths(paths));
        }
        if outcome.split_memory_or_home {
            return self.need(Phase::LayoutTail, LaunchNeed::RestoreSplitOrHome);
        }
        let front = outcome
            .front
            .as_ref()
            .and_then(|key| self.ids.get(key))
            .is_some_and(|id| !id.is_empty());
        let split = match deps.manager().split_state() {
            Ok(split) => split,
            Err(error) => return self.layout_failed(editors, error),
        };
        if !front && !split {
            return self.need(Phase::LayoutTail, LaunchNeed::OpenHome);
        }
        self.need(Phase::Settled, LaunchNeed::Settled)
    }

    fn layout_failed<E: RecoveryEditors>(self, editors: &E, error: Failure) -> LaunchStep {
        editors.warn(
            "[recovery] the restored session could not be laid out:",
            &error.0,
        );
        self.need(Phase::FallbackHome, LaunchNeed::OpenHome)
    }
}
