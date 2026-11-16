//! Prepared state-only startup tab mutations. Publishing is the caller's
//! separate captured-authority operation, never a side effect of the reducer.
use super::*;

#[derive(PartialEq)]
struct Stamp<V> {
    rows: Vec<(
        String,
        TabKind,
        Option<V>,
        String,
        Option<String>,
        bool,
        Option<Opener>,
    )>,
    active: String,
    split: Option<SplitViewState>,
    next_id: u64,
}

impl<V: Clone + PartialEq, B: Clone + PartialEq> TabManager<V, B> {
    fn startup_stamp(&self) -> Stamp<V> {
        Stamp {
            rows: self
                .tabs
                .iter()
                .map(|tab| {
                    (
                        tab.id.clone(),
                        tab.kind,
                        tab.view.clone(),
                        tab.title.clone(),
                        tab.file_path.clone(),
                        tab.present,
                        tab.opener,
                    )
                })
                .collect(),
            active: self.active_id.clone(),
            split: self.split.clone(),
            next_id: self.next_id,
        }
    }

    /// `untitled` is the already captured localized fallback, not a host callback.
    pub fn prepare_startup_add(
        &self,
        spec: UnloadedTabSpec,
        untitled: String,
    ) -> PreparedStartupMutation<V> {
        let title = spec
            .title
            .clone()
            .unwrap_or_else(|| match truthy(spec.file_path.as_deref()) {
                Some(path) => basename(path, None),
                None => untitled,
            });
        PreparedStartupMutation {
            expected: self.startup_stamp(),
            mutation: Mutation::Add { spec, title },
        }
    }

    pub fn prepare_startup_reorder(&self, id: &str, index: f64) -> PreparedStartupMutation<V> {
        PreparedStartupMutation {
            expected: self.startup_stamp(),
            mutation: Mutation::Reorder {
                id: id.to_owned(),
                index,
            },
        }
    }

    /// Prepare the `hydrateSession({tabs})` append as one published change.
    pub fn prepare_startup_hydrate(
        &self,
        specs: Vec<UnloadedTabSpec>,
    ) -> PreparedStartupHydration<V> {
        PreparedStartupHydration {
            expected: self.startup_stamp(),
            specs,
        }
    }

    /// Bind every sealed opener before changing the tab strip. The binder must
    /// undo its own partial bindings when it returns an error.
    pub fn commit_startup_hydrate(
        &mut self,
        prepared: PreparedStartupHydration<V>,
        bind: &mut dyn FnMut(&[(Opener, String)]) -> Result<(), String>,
    ) -> Result<Vec<String>, String> {
        if self.startup_stamp() != prepared.expected {
            return Err("The startup tab state changed before hydration".into());
        }
        if prepared.specs.iter().any(|spec| spec.title.is_none()) {
            return Err("A recovery tab must have its captured title".into());
        }
        let count = u64::try_from(prepared.specs.len()).map_err(|_| "Too many recovery tabs")?;
        self.next_id
            .checked_add(count)
            .ok_or("Recovery tab identifiers exhausted")?;
        let bindings: Vec<_> = prepared
            .specs
            .iter()
            .enumerate()
            .map(|(index, spec)| (spec.opener, format!("t{}", self.next_id + index as u64)))
            .collect();
        bind(&bindings)?;
        let mut ids = Vec::with_capacity(bindings.len());
        for (spec, (_, id)) in prepared.specs.into_iter().zip(bindings) {
            self.next_id += 1;
            self.tabs.push(TabRecord {
                id: id.clone(),
                kind: spec.kind,
                view: None,
                binding: None,
                title: spec.title.unwrap(),
                file_path: spec.file_path,
                present: false,
                opener: Some(spec.opener),
                lazy: None,
            });
            ids.push(id);
        }
        Ok(ids)
    }

    /// The returned outcome describes the committed state only; the caller
    /// must publish the captured change and await its actual completion.
    pub fn commit_startup_mutation(
        &mut self,
        prepared: PreparedStartupMutation<V>,
        bind: &mut dyn FnMut(Opener, &str) -> Result<(), String>,
    ) -> Result<StartupMutationResult, String> {
        if self.startup_stamp() != prepared.expected {
            return Err("The startup tab state changed before commit".into());
        }
        match prepared.mutation {
            Mutation::Add { spec, title } => {
                let prospective = format!("t{}", self.next_id);
                bind(spec.opener, &prospective)?;
                let id = self.alloc_id();
                self.tabs.push(TabRecord {
                    id: id.clone(),
                    kind: spec.kind,
                    view: None,
                    binding: None,
                    title,
                    file_path: spec.file_path,
                    present: false,
                    opener: Some(spec.opener),
                    lazy: None,
                });
                Ok(StartupMutationResult {
                    id: Some(id),
                    changed: true,
                })
            }
            Mutation::Reorder { id, index } => {
                if id == HOME_ID {
                    return Ok(StartupMutationResult {
                        id: None,
                        changed: false,
                    });
                }
                let Some(from) = self.index_of(&id) else {
                    return Ok(StartupMutationResult {
                        id: None,
                        changed: false,
                    });
                };
                let clamped = math_min(math_max(index.trunc(), 1.0), (self.tabs.len() - 1) as f64);
                if clamped == from as f64 {
                    return Ok(StartupMutationResult {
                        id: None,
                        changed: false,
                    });
                }
                let moved = self.tabs.remove(from);
                let at = if clamped.is_nan() {
                    0
                } else {
                    (clamped as usize).min(self.tabs.len())
                };
                self.tabs.insert(at, moved);
                Ok(StartupMutationResult {
                    id: None,
                    changed: true,
                })
            }
        }
    }
}

pub struct PreparedStartupMutation<V> {
    expected: Stamp<V>,
    mutation: Mutation,
}

pub struct PreparedStartupHydration<V> {
    expected: Stamp<V>,
    specs: Vec<UnloadedTabSpec>,
}

enum Mutation {
    Add {
        spec: UnloadedTabSpec,
        title: String,
    },
    Reorder {
        id: String,
        index: f64,
    },
}

pub struct StartupMutationResult {
    pub id: Option<String>,
    pub changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: u64) -> UnloadedTabSpec {
        UnloadedTabSpec {
            kind: TabKind::Docs,
            title: Some("Recovered".into()),
            file_path: None,
            opener: Opener {
                id,
                has_needs_user: false,
            },
        }
    }

    #[test]
    fn rejected_binding_does_not_allocate_or_insert_and_retry_uses_same_id() {
        let mut tabs = TabManager::<u64, ()>::new();
        let prepared = tabs.prepare_startup_add(spec(7), String::new());
        let error = tabs.commit_startup_mutation(prepared, &mut |opener, id| {
            assert_eq!(opener.id, 7);
            assert_eq!(id, "t1");
            Err("binding rejected".into())
        });
        assert!(error.is_err());
        assert_eq!(tabs.list().len(), 1);
        let prepared = tabs.prepare_startup_add(spec(8), String::new());
        let result = tabs
            .commit_startup_mutation(prepared, &mut |_, id| {
                assert_eq!(id, "t1");
                Ok(())
            })
            .unwrap();
        assert_eq!(result.id.as_deref(), Some("t1"));
        assert_eq!(tabs.list().len(), 2);
    }

    #[test]
    fn stale_preparation_rejects_before_binding() {
        let mut tabs = TabManager::<u64, ()>::new();
        let old = tabs.prepare_startup_add(spec(1), String::new());
        let fresh = tabs.prepare_startup_add(spec(2), String::new());
        tabs.commit_startup_mutation(fresh, &mut |_, _| Ok(()))
            .unwrap();
        let mut called = false;
        assert!(
            tabs.commit_startup_mutation(old, &mut |_, _| {
                called = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!called);
        assert_eq!(tabs.list().len(), 2);
    }

    #[test]
    fn hydration_binds_the_whole_batch_before_mutating() {
        let mut tabs = TabManager::<u64, ()>::new();
        let prepared = tabs.prepare_startup_hydrate(vec![spec(1), spec(2)]);
        assert!(
            tabs.commit_startup_hydrate(prepared, &mut |bindings| {
                assert_eq!(
                    bindings
                        .iter()
                        .map(|(_, id)| id.as_str())
                        .collect::<Vec<_>>(),
                    vec!["t1", "t2"]
                );
                Err("binding failed".into())
            })
            .is_err()
        );
        assert_eq!(tabs.list().len(), 1);
        let prepared = tabs.prepare_startup_hydrate(vec![spec(1), spec(2)]);
        assert_eq!(
            tabs.commit_startup_hydrate(prepared, &mut |_| Ok(()))
                .unwrap(),
            vec!["t1", "t2"]
        );
        assert_eq!(tabs.list().len(), 3);
    }

    #[test]
    fn stale_hydration_never_binds_any_opener() {
        let mut tabs = TabManager::<u64, ()>::new();
        let stale = tabs.prepare_startup_hydrate(vec![spec(1), spec(2)]);
        let change = tabs.prepare_startup_add(spec(3), String::new());
        tabs.commit_startup_mutation(change, &mut |_, _| Ok(()))
            .unwrap();
        let mut called = false;
        assert!(
            tabs.commit_startup_hydrate(stale, &mut |_| {
                called = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!called);
        assert_eq!(tabs.list().len(), 2);
    }

    #[test]
    fn hydration_rejects_missing_captured_title_before_binding() {
        let mut tabs = TabManager::<u64, ()>::new();
        let mut missing = spec(1);
        missing.title = None;
        let prepared = tabs.prepare_startup_hydrate(vec![missing]);
        let mut called = false;
        assert!(
            tabs.commit_startup_hydrate(prepared, &mut |_| {
                called = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!called);
        assert_eq!(tabs.list().len(), 1);
    }
}
