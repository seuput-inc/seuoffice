use super::*;

pub struct PreparedDocsClose<V> { id: String, view: V }
impl<V> PreparedDocsClose<V> {
    pub fn id(&self) -> &str { &self.id }
    pub fn view(&self) -> &V { &self.view }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabRemovalState {
    pub activate: Option<String>,
    pub layout: bool,
    pub split_changed: bool,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GeneratedRemovalSurvivor<V> {
    Loaded { id: String, view: V },
    Unloaded { opener: Opener, context: UnloadedTabOpenContext },
    Viewless { id: String },
}

/// The state that would result from removing one captured generated tab.
/// This snapshot contains no authority to commit the removal.
pub struct GeneratedRemovalPreview<V, B> {
    manager: TabManager<V, B>,
    removal: TabRemovalState,
}

impl<V, B> GeneratedRemovalPreview<V, B> {
    pub fn manager(&self) -> &TabManager<V, B> { &self.manager }
    pub fn manager_mut(&mut self) -> &mut TabManager<V, B> { &mut self.manager }
    pub fn removal(&self) -> &TabRemovalState { &self.removal }
}

impl<V: Clone + PartialEq, B: Clone + PartialEq> GeneratedRemovalPreview<V, B> {
    /// Identities are captured before close; unloaded rows retain their opener
    /// and inputs, not a fabricated renderer owner.
    pub fn survivors(&self) -> Vec<GeneratedRemovalSurvivor<V>> {
        self.manager.tabs.iter().enumerate().map(|(index, tab)| {
            if let Some(view) = &tab.view {
                GeneratedRemovalSurvivor::Loaded { id: tab.id.clone(), view: view.clone() }
            } else if let Some(opener) = tab.opener {
                GeneratedRemovalSurvivor::Unloaded { opener, context: self.manager.open_context(index) }
            } else {
                GeneratedRemovalSurvivor::Viewless { id: tab.id.clone() }
            }
        }).collect()
    }
}


pub struct StagedDocsClose<V> {
    close: PreparedDocsClose<V>,
    pub removal: TabRemovalState,
    pub closed_path: Option<String>,
}

impl<V> StagedDocsClose<V> {
    pub fn id(&self) -> &str { self.close.id() }
    pub fn view(&self) -> &V { self.close.view() }
}

impl<V: Clone + PartialEq, B: Clone + PartialEq> TabManager<V, B> {
    pub fn prepare_docs_close_owned(&mut self,id:&str) -> Option<PreparedDocsClose<V>> {
        if self.is_closing(id) { return None; }
        let tab = self.tab_by_id(id)?;
        if tab.kind != TabKind::Docs { return None; }
        let view = tab.view.clone()?;
        self.pending_closes.push((id.to_owned(),ClosePhase::Query { view:view.clone() }));
        Some(PreparedDocsClose { id:id.to_owned(),view })
    }

    fn owns_docs_close(&self,close:&PreparedDocsClose<V>) -> bool {
        self.pending_closes.iter().any(|(id,phase)| id == &close.id && matches!(phase,ClosePhase::Query { view } if view == &close.view))
            && self.tab_by_id(&close.id).is_some_and(|tab| tab.kind == TabKind::Docs && tab.view.as_ref() == Some(&close.view))
    }

    pub fn begin_docs_close_prompt_owned(&mut self,close:&PreparedDocsClose<V>) -> Result<(),String> {
        if !self.owns_docs_close(close) { return Err("The document close no longer owns its tab".into()); }
        self.begin_exclusive_prompt();
        Ok(())
    }

    pub fn end_exclusive_prompt_state(&mut self) -> bool {
        if self.prompt_depth == 0 { return false; }
        self.prompt_depth -= 1;
        if self.prompt_depth > 0 { return false; }
        let split = self.prompt_split_snapshot.take();
        let Some(active) = self.prompt_active_id_snapshot.take() else { return false; };
        if self.split == split && self.active_id == active { return false; }
        self.split = split;
        if self.tabs.iter().any(|tab| tab.id == active) { self.active_id = active; }
        true
    }

    pub fn cancel_docs_close_owned(&mut self,close:PreparedDocsClose<V>) {
        self.pending_closes.retain(|(id,phase)| !(id == &close.id && matches!(phase,ClosePhase::Query { view } if view == &close.view)));
    }

    /// Commits logical removal while retaining the close reservation until the
    /// captured native browser and renderer have actually finished disposal.
    pub fn stage_docs_close_owned(&mut self,close:PreparedDocsClose<V>) -> Result<StagedDocsClose<V>,String> {
        if !self.owns_docs_close(&close) {
            self.cancel_docs_close_owned(close);
            return Err("The document close no longer matches its tab".into());
        }
        let (idx,tab) = self.take_removed_tab(&close.id).ok_or("The document tab disappeared")?;
        let closed_path = if !tab.present { tab.file_path.filter(|path| !path.is_empty()) } else { None };
        let removal = self.after_leaving_strip_state(&close.id,idx);
        Ok(StagedDocsClose { close, removal, closed_path })
    }

    pub fn acknowledge_docs_closed_owned(&mut self,close:&StagedDocsClose<V>,view:&V) -> Result<(),String> {
        if close.close.view != *view || !self.pending_closes.iter().any(|(id,phase)|
            id == &close.close.id && matches!(phase,ClosePhase::Query { view } if view == &close.close.view)) {
            return Err("The disposed document no longer matches its close reservation".into());
        }
        self.pending_closes.retain(|(id,phase)| !(id == &close.close.id
            && matches!(phase,ClosePhase::Query { view } if view == &close.close.view)));
        Ok(())
    }

    /// Preview removal of the editor that a validated adoption would add,
    /// without publishing the adoption or consuming its single-use plan.
    pub fn preview_prepared_generated_adoption(
        &self,
        prepared: &PreparedEditorAdoption<V, B>,
        commit: &PreparedEditorAdoptionCommit<V>,
        binding: &B,
    ) -> Result<GeneratedRemovalPreview<V, B>, String> {
        self.validate_editor_adoption_commit(prepared, commit)?;
        let mut manager = self.clone();
        manager.next_id += 1;
        if let Some(view) = &commit.full_screen { manager.full_screen_tracked.push(view.clone()); }
        if let Some(view) = &commit.pane_focus { manager.pane_focus_tracked.push(view.clone()); }
        manager.tabs.push(TabRecord {
            id: commit.id.clone(), kind: prepared.kind, view: Some(prepared.view.clone()),
            binding: Some(binding.clone()), title: prepared.title.clone(), file_path: prepared.file_path.clone(),
            present: false, opener: None, lazy: None,
        });
        let removal = manager.remove_closed_generated_owned(&commit.result_id, &prepared.view)?;
        Ok(GeneratedRemovalPreview { manager, removal })
    }

    /// Prepare post-removal effects while the generated renderer is still alive.
    /// The actual removal must still validate the original closure reservation.
    pub fn preview_closed_generated_owned(&self, id: &str, view: &V) -> Result<GeneratedRemovalPreview<V, B>, String> {
        let mut manager = self.clone();
        let removal = manager.remove_closed_generated_owned(id, view)?;
        Ok(GeneratedRemovalPreview { manager, removal })
    }

    /// Called only after the exact generated view's native close receipt.
    pub fn remove_closed_generated_owned(&mut self, id: &str, view: &V) -> Result<TabRemovalState, String> {
        if id == HOME_ID || self.is_closing(id)
            || !self.tab_by_id(id).is_some_and(|tab| tab.view.as_ref() == Some(view)) {
            return Err("The closed generated view no longer matches its tab".into());
        }
        let (index, _) = self.take_removed_tab(id).ok_or("The closed generated tab disappeared")?;
        Ok(self.after_leaving_strip_state(id, index))
    }

    /// Commit a receipt-validated removal and its prepared activation together.
    /// Validation happens on a private clone, so stale state leaves the live
    /// manager and the one-shot activation intact.
    pub fn commit_prepared_generated_removal(
        &mut self, id: &str, view: &V, expected: &TabRemovalState,
        prepared: &mut Option<PreparedTabActivation<V, B>>,
    ) -> Result<Vec<TabActivationEffect<V, B>>, String> {
        let mut next = self.clone();
        let actual = next.remove_closed_generated_owned(id, view)?;
        if &actual != expected { return Err("The generated removal state changed".into()); }
        let activation = prepared.as_ref().ok_or("The generated removal was already committed")?;
        next.validate_loaded_activation(activation)?;
        let activation = prepared.take().ok_or("The generated removal was already committed")?;
        let effects = next.commit_loaded_activation(activation)?;
        *self = next;
        Ok(effects)
    }

    fn take_removed_tab(&mut self,id:&str) -> Option<(usize,TabRecord<V, B>)> {
        let idx = self.index_of(id)?;
        if self.html_full_screen_id.as_deref() == Some(id) { self.html_full_screen_id = None; }
        Some((idx,self.tabs.remove(idx)))
    }

    fn after_leaving_strip_state(&mut self,id:&str,idx:usize) -> TabRemovalState {
        let survivor = self.split.as_ref().and_then(|split| split_survivor(&split.a_tab_id,&split.b_tab_id,id)).map(|(_,id)| id.to_owned());
        if let Some(survivor) = survivor {
            self.split = None;
            let activate = (self.active_id == id && self.tabs.iter().any(|tab| tab.id == survivor)).then_some(survivor);
            return TabRemovalState { layout:activate.is_none(),activate,split_changed:true,changed:true };
        }
        let activate = if self.active_id == id {
            idx.checked_sub(1).and_then(|idx| self.tabs.get(idx)).or_else(|| self.tabs.first()).map(|tab| tab.id.clone())
        } else { None };
        TabRemovalState { activate,layout:false,split_changed:false,changed:self.active_id != id }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(manager:&mut TabManager<u32, ()>,id:&str,view:u32) {
        manager.tabs.push(TabRecord { id:id.into(),kind:TabKind::Docs,view:Some(view),binding:Some(()),title:id.into(),file_path:Some(format!("/{id}.docx")),present:false,opener:None,lazy:None });
    }

    #[test]
    fn closed_background_document_preserves_active_tab_and_original_path() {
        let mut manager = TabManager::new();
        add(&mut manager,"docs",1);
        let close = manager.prepare_docs_close_owned("docs").unwrap();
        let ack = manager.stage_docs_close_owned(close).unwrap();
        assert_eq!(manager.active_id,HOME_ID);
        assert_eq!(ack.closed_path.as_deref(),Some("/docs.docx"));
        assert!(ack.removal.activate.is_none());
        assert!(ack.removal.changed);
        assert!(manager.web_contents_for_tab("docs").is_none());
        assert!(manager.is_closing("docs"));
        assert!(manager.prepare_docs_close_owned("docs").is_none());
        assert!(manager.acknowledge_docs_closed_owned(&ack, &2).is_err());
        assert!(manager.is_closing("docs"));
        manager.acknowledge_docs_closed_owned(&ack, &1).unwrap();
        assert!(!manager.is_closing("docs"));
    }

    #[test]
    fn replacement_cannot_be_removed_by_predecessor_cleanup_receipt() {
        let mut manager = TabManager::new();
        add(&mut manager,"docs",1);
        let close = manager.prepare_docs_close_owned("docs").unwrap();
        manager.tabs.iter_mut().find(|tab|tab.id=="docs").unwrap().view=Some(2);
        assert!(manager.stage_docs_close_owned(close).is_err());
        assert_eq!(manager.web_contents_for_tab("docs"),Some(2));
        assert!(manager.prepare_docs_close_owned("docs").is_some());
    }

    #[test]
    fn generated_close_receipt_cannot_remove_a_replacement() {
        let mut manager = TabManager::new();
        add(&mut manager, "generated", 7);
        assert!(manager.remove_closed_generated_owned("generated", &8).is_err());
        assert_eq!(manager.web_contents_for_tab("generated"), Some(7));
        manager.remove_closed_generated_owned("generated", &7).unwrap();
        assert!(manager.web_contents_for_tab("generated").is_none());
        assert!(manager.remove_closed_generated_owned("generated", &7).is_err());
    }

    #[test]
    fn generated_removal_preview_does_not_remove_or_relabel_the_live_entry() {
        let mut manager = TabManager::new();
        add(&mut manager, "generated", 7);
        manager.active_id = "generated".into();
        let preview = manager.preview_closed_generated_owned("generated", &7).unwrap();
        assert_eq!(manager.web_contents_for_tab("generated"), Some(7));
        assert_eq!(preview.manager().web_contents_for_tab("generated"), None);
        assert_eq!(preview.removal().activate.as_deref(), Some(HOME_ID));
        manager.tabs.iter_mut().find(|tab| tab.id == "generated").unwrap().view = Some(8);
        assert!(manager.remove_closed_generated_owned("generated", &7).is_err());
        assert_eq!(manager.web_contents_for_tab("generated"), Some(8));
    }

    #[test]
    fn generated_background_removal_prepares_changed_without_native_layout() {
        let mut manager = TabManager::new();
        add(&mut manager, "generated", 7);
        let preview = manager.preview_closed_generated_owned("generated", &7).unwrap();
        let env = ActivationEnvironment {
            window_destroyed: false, window_focused: true, window_size: (1000.0, 700.0),
            ui_scale: 1.0, no_spare_view: false, target_bleeds: false,
        };
        let effects = preview.manager().prepare_generated_removal_effects(preview.removal(), env).unwrap();
        assert_eq!(effects.effects(), &[TabActivationEffect::Changed]);
        assert_eq!(manager.web_contents_for_tab("generated"), Some(7));
    }

    #[test]
    fn split_removal_prepares_changed_before_split_publication() {
        let mut manager = TabManager::new();
        add(&mut manager, "survivor", 8);
        add(&mut manager, "generated", 7);
        manager.active_id = "survivor".into();
        manager.split = Some(SplitViewState {
            a_tab_id: "survivor".into(), b_tab_id: "generated".into(),
            orientation: SplitOrientation::Side, ratio: 0.5, focus: Pane::A,
        });
        let preview = manager.preview_closed_generated_owned("generated", &7).unwrap();
        let env = ActivationEnvironment {
            window_destroyed: false, window_focused: true, window_size: (1000.0, 700.0),
            ui_scale: 1.0, no_spare_view: false, target_bleeds: false,
        };
        let effects = preview.manager().prepare_generated_removal_effects(preview.removal(), env).unwrap();
        let changed = effects.effects().iter().position(|effect| matches!(effect, TabActivationEffect::Changed)).unwrap();
        let split = effects.effects().iter().position(|effect| matches!(effect, TabActivationEffect::SplitChanged(None))).unwrap();
        assert!(changed < split);
        assert!(manager.split.is_some());
    }

    #[test]
    fn generated_preview_distinguishes_home_unloaded_and_loaded_survivors() {
        let mut manager = TabManager::new();
        manager.tabs.push(TabRecord {
            id: "lazy".into(), kind: TabKind::Docs, view: None, binding: None, title: "Lazy".into(),
            file_path: Some("/lazy.docx".into()), present: false,
            opener: Some(Opener { id: 42, has_needs_user: true }), lazy: None,
        });
        add(&mut manager, "loaded", 8);
        add(&mut manager, "generated", 7);
        let preview = manager.preview_closed_generated_owned("generated", &7).unwrap();
        let survivors = preview.survivors();
        assert!(survivors.iter().any(|entry| matches!(entry, GeneratedRemovalSurvivor::Viewless { id } if id == HOME_ID)));
        assert!(survivors.iter().any(|entry| matches!(entry, GeneratedRemovalSurvivor::Unloaded { opener, context } if opener.id == 42 && context.tab_id == "lazy")));
        assert!(survivors.iter().any(|entry| matches!(entry, GeneratedRemovalSurvivor::Loaded { id, view } if id == "loaded" && *view == 8)));
    }

    #[test]
    fn captured_unloaded_survivor_cannot_hydrate_a_replaced_opener() {
        let mut manager = TabManager::new();
        manager.tabs.push(TabRecord {
            id: "lazy".into(), kind: TabKind::Docs, view: None, binding: None, title: "Lazy".into(),
            file_path: Some("/lazy.docx".into()), present: false,
            opener: Some(Opener { id: 42, has_needs_user: false }), lazy: None,
        });
        add(&mut manager, "generated", 7);
        let preview = manager.preview_closed_generated_owned("generated", &7).unwrap();
        let captured = preview.survivors().into_iter().find_map(|entry| match entry {
            GeneratedRemovalSurvivor::Unloaded { opener, context } => Some((opener, context)),
            _ => None,
        }).unwrap();
        manager.tabs.iter_mut().find(|tab| tab.id == "lazy").unwrap().opener =
            Some(Opener { id: 43, has_needs_user: false });
        assert!(manager.prepare_captured_tab_load(&captured.0, &captured.1).is_err());
        assert!(manager.loading_ids.is_empty());
    }

    #[test]
    fn captured_survivor_reservation_is_single_flight_and_checks_context() {
        let mut manager = TabManager::new();
        manager.tabs.push(TabRecord {
            id: "lazy".into(), kind: TabKind::Docs, view: None, binding: None, title: "Lazy".into(),
            file_path: Some("/lazy.docx".into()), present: false,
            opener: Some(Opener { id: 42, has_needs_user: false }), lazy: None,
        });
        add(&mut manager, "generated", 7);
        let preview = manager.preview_closed_generated_owned("generated", &7).unwrap();
        let (opener, context) = preview.survivors().into_iter().find_map(|entry| match entry {
            GeneratedRemovalSurvivor::Unloaded { opener, context } => Some((opener, context)),
            _ => None,
        }).unwrap();
        let plan = manager.prepare_captured_tab_load(&opener, &context).unwrap();
        assert!(manager.prepare_captured_tab_load(&opener, &context).is_err());
        manager.cancel_tab_load(plan);
        manager.tabs.iter_mut().find(|tab| tab.id == "lazy").unwrap().file_path = Some("/replacement.docx".into());
        assert!(manager.prepare_captured_tab_load(&opener, &context).is_err());
        assert!(manager.loading_ids.is_empty());
    }

    #[test]
    fn intervening_tab_change_does_not_remove_or_consume_the_prepared_transition() {
        let mut manager = TabManager::new();
        add(&mut manager, "generated", 7);
        let preview = manager.preview_closed_generated_owned("generated", &7).unwrap();
        let env = ActivationEnvironment {
            window_destroyed: false, window_focused: true, window_size: (1000.0, 700.0),
            ui_scale: 1.0, no_spare_view: false, target_bleeds: false,
        };
        let mut prepared = Some(preview.manager().prepare_generated_removal_effects(preview.removal(), env).unwrap());
        add(&mut manager, "newer", 8);
        assert!(manager.commit_prepared_generated_removal("generated", &7, preview.removal(), &mut prepared).is_err());
        assert_eq!(manager.web_contents_for_tab("generated"), Some(7));
        assert_eq!(manager.web_contents_for_tab("newer"), Some(8));
        assert!(prepared.is_some());
    }

    #[test]
    fn prepared_generated_removal_commits_once_and_returns_effects_in_order() {
        let mut manager = TabManager::new();
        add(&mut manager, "generated", 7);
        let preview = manager.preview_closed_generated_owned("generated", &7).unwrap();
        let env = ActivationEnvironment {
            window_destroyed: false, window_focused: true, window_size: (1000.0, 700.0),
            ui_scale: 1.0, no_spare_view: false, target_bleeds: false,
        };
        let mut prepared = Some(preview.manager().prepare_generated_removal_effects(preview.removal(), env).unwrap());
        let effects = manager.commit_prepared_generated_removal("generated", &7, preview.removal(), &mut prepared).unwrap();
        assert_eq!(effects, vec![TabActivationEffect::Changed]);
        assert!(prepared.is_none());
        assert!(manager.web_contents_for_tab("generated").is_none());
        assert!(manager.commit_prepared_generated_removal("generated", &7, preview.removal(), &mut prepared).is_err());
    }
}
