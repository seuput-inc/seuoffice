//! Prepared activation changes only tab state; adapters execute its ordered effects.
use super::*;

#[derive(Clone, Copy, Debug)]
pub struct ActivationEnvironment {
    pub window_destroyed: bool,
    pub window_focused: bool,
    pub window_size: (f64, f64),
    pub ui_scale: f64,
    pub no_spare_view: bool,
    pub target_bleeds: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TabActivationEffect<V, B> {
    SetSpareTimer(f64),
    ClearSpareTimer,
    RemoveChild(V, B),
    Close(V, B),
    Visible(V, bool),
    Bounds(V, Rect),
    ActiveDocs(Option<V>),
    ActiveSheets(V),
    ActiveSlides(V),
    Menu(TabKind),
    Focus(V),
    FocusShell,
    Changed,
    SplitChanged(Option<SplitViewState>),
}

impl<V, B> TabActivationEffect<V, B> {
    pub fn view(&self) -> Option<&V> {
        match self {
            Self::RemoveChild(view, _) | Self::Close(view, _) | Self::Visible(view, _)
            | Self::Bounds(view, _) | Self::ActiveSheets(view) | Self::ActiveSlides(view)
            | Self::Focus(view) => Some(view),
            Self::ActiveDocs(view) => view.as_ref(),
            _ => None,
        }
    }

    /// Synchronous/native-only callers retain the original host sequence.
    /// Request-bound adapters instead bind these effects to captured native handles.
    pub fn apply(self, host: &mut dyn TabHost<V, B>) {
        match self {
            Self::SetSpareTimer(delay) => host.set_spare_timer(delay),
            Self::ClearSpareTimer => host.clear_spare_timer(),
            Self::RemoveChild(view, _) => host.remove_child_view(&view),
            Self::Close(view, _) => host.close_view(&view),
            Self::Visible(view, visible) => host.set_visible(&view, visible),
            Self::Bounds(view, bounds) => host.set_bounds(&view, &bounds),
            Self::ActiveDocs(view) => host.set_active_docs_resolver(view.as_ref(), false),
            Self::ActiveSheets(view) => host.set_active_sheets_web_contents(Some(&view)),
            Self::ActiveSlides(view) => host.set_active_slides_web_contents(&view),
            Self::Menu(kind) => host.apply_menu_for(kind),
            Self::Focus(view) => host.focus_view(&view),
            Self::FocusShell => host.focus_shell_contents(),
            Self::Changed => host.on_changed(),
            Self::SplitChanged(split) => host.on_split_changed(split.as_ref()),
        }
    }
}

#[derive(PartialEq)]
struct ActivationStamp<V, B> {
    active: String,
    previous: Option<String>,
    split: Option<SplitViewState>,
    spare: Option<(V, B)>,
    spare_timer: bool,
    full_screen: Option<String>,
    bleed: Vec<i64>,
    tabs: Vec<(String, TabKind, Option<V>, Option<B>, bool, Option<Opener>)>,
}

struct ActivationState<V, B> {
    active: String,
    previous: Option<String>,
    split: Option<SplitViewState>,
    spare: Option<(V, B)>,
    spare_timer: bool,
    bleed: Vec<i64>,
}

/// Non-Clone, one-shot tab-state transition plus an immutable ordered effect batch.
/// Preparing this value does not change the manager or invoke a native effect.
pub struct PreparedTabActivation<V, B> {
    expected: ActivationStamp<V, B>,
    next: ActivationState<V, B>,
    effects: Vec<TabActivationEffect<V, B>>,
}

impl<V, B> PreparedTabActivation<V, B> {
    pub fn effects(&self) -> &[TabActivationEffect<V, B>] { &self.effects }
    pub fn target(&self) -> &str { &self.next.active }
}

impl<V: Clone + PartialEq, B: Clone + PartialEq> TabManager<V, B> {
    fn activation_stamp(&self) -> ActivationStamp<V, B> {
        ActivationStamp {
            active: self.active_id.clone(), previous: self.previous_active_id.clone(),
            split: self.split.clone(), spare: self.spare_sheets_view.clone(),
            spare_timer: self.spare_sheets_timer, full_screen: self.html_full_screen_id.clone(),
            bleed: self.bleed_wc_ids.clone(), tabs: self.tabs.iter().map(|tab|
                (tab.id.clone(), tab.kind, tab.view.clone(), tab.binding.clone(), tab.present, tab.opener)).collect(),
        }
    }

    /// Capture environmental reads before preparing/admitting the state transition.
    pub fn activation_environment(&self, host: &mut dyn TabHost<V, B>, id: &str) -> ActivationEnvironment {
        let target_bleeds = self.tab_by_id(id).and_then(|tab| tab.view.as_ref())
            .is_some_and(|view| self.bleed_wc_ids.contains(&host.view_id(view)));
        ActivationEnvironment {
            window_destroyed: host.window_destroyed(), window_focused: host.window_focused(),
            window_size: host.window_content_size(), ui_scale: host.ui_scale(),
            no_spare_view: host.no_spare_view_env(), target_bleeds,
        }
    }

    /// The opener/hydration must already have completed. This never creates/reloads a view.
    pub fn prepare_loaded_activation(&self, id: &str, environment: ActivationEnvironment) -> Result<PreparedTabActivation<V, B>, String> {
        let target = self.tab_by_id(id).ok_or("the target tab is no longer present")?;
        if target.opener.is_some() { return Err("the target tab has not completed hydration".into()); }
        let mut next = ActivationState {
            active: id.to_owned(), previous: self.previous_active_id.clone(), split: self.split.clone(),
            spare: self.spare_sheets_view.clone(), spare_timer: self.spare_sheets_timer,
            bleed: self.bleed_wc_ids.clone(),
        };
        if self.active_id != id { next.previous = Some(self.active_id.clone()); }
        let mut effects = Vec::with_capacity(self.tabs.len() + 16);
        let mut replace_pane = false;
        if let Some(split) = next.split.clone() {
            match activation_mode(Some((&split.a_tab_id, &split.b_tab_id)), id, target.kind.as_str(), target.present) {
                SplitActivationMode::ReplacePane => {
                    replace_pane = true;
                    next.split = Some(if split.focus == Pane::A {
                        SplitViewState { a_tab_id: id.to_owned(), ..split }
                    } else { SplitViewState { b_tab_id: id.to_owned(), ..split } });
                }
                SplitActivationMode::Pane => {
                    let side = if id == split.a_tab_id { Pane::A } else { Pane::B };
                    if side != split.focus {
                        next.split = Some(SplitViewState { focus: side, ..split });
                        effects.push(TabActivationEffect::SplitChanged(next.split.clone()));
                    }
                }
                _ => {}
            }
        }
        if !replace_pane {
            if target.kind == TabKind::Sheets && next.split.is_none() {
                if !environment.no_spare_view && !next.spare_timer && next.spare.is_none() {
                    effects.push(TabActivationEffect::SetSpareTimer(3000.0));
                    next.spare_timer = true;
                }
            } else {
                if next.spare_timer { effects.push(TabActivationEffect::ClearSpareTimer); }
                next.spare_timer = false;
                if let Some((view, binding)) = next.spare.take() {
                    effects.push(TabActivationEffect::RemoveChild(view.clone(), binding.clone()));
                    effects.push(TabActivationEffect::Close(view, binding));
                }
            }
        }
        self.append_activation_layout(id, &mut next.split, environment, &mut effects);
        let menu = if target.present { TabKind::Home } else { target.kind };
        effects.push(TabActivationEffect::ActiveDocs(if target.kind == TabKind::Docs { target.view.clone() } else { None }));
        if let Some(view) = &target.view {
            if target.kind == TabKind::Sheets { effects.push(TabActivationEffect::ActiveSheets(view.clone())); }
            if target.kind == TabKind::Slides { effects.push(TabActivationEffect::ActiveSlides(view.clone())); }
        }
        effects.push(TabActivationEffect::Menu(menu));
        if !environment.window_destroyed && environment.window_focused {
            effects.push(match &target.view {
                Some(view) => TabActivationEffect::Focus(view.clone()),
                None => TabActivationEffect::FocusShell,
            });
        }
        effects.push(TabActivationEffect::Changed);
        if replace_pane { effects.push(TabActivationEffect::SplitChanged(next.split.clone())); }
        Ok(PreparedTabActivation { expected: self.activation_stamp(), next, effects })
    }

    /// Prepare beginSplit after both exact pane views have completed hydration.
    pub fn prepare_loaded_split(
        &self, a_id: &str, b_id: &str, orientation: SplitOrientation, ratio: f64, focus: Pane,
        environment: ActivationEnvironment,
    ) -> Result<Option<PreparedTabActivation<V, B>>, String> {
        if a_id == b_id { return Ok(None); }
        let (Some(a), Some(b)) = (self.tab_by_id(a_id), self.tab_by_id(b_id)) else { return Ok(None); };
        if a.present || b.present || matches!(a.kind, TabKind::Home | TabKind::Help)
            || matches!(b.kind, TabKind::Home | TabKind::Help) { return Ok(None); }
        if (a.view.is_none() && a.opener.is_none()) || (b.view.is_none() && b.opener.is_none()) { return Ok(None); }
        if a.opener.is_some() || b.opener.is_some() {
            return Err("the split panes have not completed hydration".into());
        }
        let mut ratio = if ratio.is_finite() { math_min(1.0, math_max(0.0, ratio)) } else { SPLIT_EVEN_RATIO };
        if ratio > 0.0 && ratio < 1.0 {
            let (width, height) = environment.window_size;
            let strip = tab_strip_height(environment.ui_scale);
            let bounds = Rect { x: 0.0, y: strip, width, height: math_max(0.0, height - strip) };
            ratio = clamp_split_ratio(ratio, &bounds, orientation);
        }
        let active = if focus == Pane::A { a_id } else { b_id };
        let mut next = ActivationState {
            active: active.to_owned(),
            previous: if self.active_id != active { Some(self.active_id.clone()) } else { self.previous_active_id.clone() },
            split: Some(SplitViewState { a_tab_id: a_id.to_owned(), b_tab_id: b_id.to_owned(), orientation, ratio, focus }),
            spare: self.spare_sheets_view.clone(), spare_timer: self.spare_sheets_timer, bleed: self.bleed_wc_ids.clone(),
        };
        let mut effects = Vec::with_capacity(self.tabs.len() + 8);
        self.append_activation_layout(active, &mut next.split, environment, &mut effects);
        self.append_refresh_focus(active, environment, &mut effects);
        effects.push(TabActivationEffect::Changed);
        effects.push(TabActivationEffect::SplitChanged(next.split.clone()));
        Ok(Some(PreparedTabActivation { expected: self.activation_stamp(), next, effects }))
    }

    /// Stage the existing layout-only bleed change without touching native views.
    pub fn prepare_content_bleed(&self, wc_id: i64, on: bool, mut environment: ActivationEnvironment, active_wc_id: Option<i64>) -> PreparedTabActivation<V, B> {
        let mut bleed = self.bleed_wc_ids.clone();
        if on {
            if !bleed.contains(&wc_id) { bleed.push(wc_id); }
        } else {
            bleed.retain(|id| *id != wc_id);
        }
        environment.target_bleeds = active_wc_id.is_some_and(|id| bleed.contains(&id));
        let mut next = ActivationState {
            active: self.active_id.clone(), previous: self.previous_active_id.clone(), split: self.split.clone(),
            spare: self.spare_sheets_view.clone(), spare_timer: self.spare_sheets_timer, bleed,
        };
        let mut effects = Vec::with_capacity(self.tabs.len() + 2);
        self.append_activation_layout(&next.active, &mut next.split, environment, &mut effects);
        PreparedTabActivation { expected: self.activation_stamp(), next, effects }
    }

    /// Prepare the current arrangement's layout without changing activation or focus.
    pub fn prepare_current_layout(&self, environment: ActivationEnvironment) -> PreparedTabActivation<V, B> {
        let mut next = ActivationState {
            active: self.active_id.clone(), previous: self.previous_active_id.clone(), split: self.split.clone(),
            spare: self.spare_sheets_view.clone(), spare_timer: self.spare_sheets_timer,
            bleed: self.bleed_wc_ids.clone(),
        };
        let mut effects = Vec::with_capacity(self.tabs.len() + 2);
        self.append_activation_layout(&next.active, &mut next.split, environment, &mut effects);
        PreparedTabActivation { expected: self.activation_stamp(), next, effects }
    }

    /// Prepare a browser focus event without calling the host or stealing focus.
    pub fn prepare_browser_focus(&self, view: &V, environment: ActivationEnvironment) -> Option<PreparedTabActivation<V, B>> {
        let target = &self.tabs[self.index_of_view(view)?];
        let split = self.split.clone()?;
        let side = if target.id == split.a_tab_id { Pane::A }
            else if target.id == split.b_tab_id { Pane::B }
            else { return None };
        if split.focus == side { return None; }
        let mut next = ActivationState {
            active: target.id.clone(),
            previous: if self.active_id == target.id { self.previous_active_id.clone() } else { Some(self.active_id.clone()) },
            split: Some(SplitViewState { focus: side, ..split }),
            spare: self.spare_sheets_view.clone(), spare_timer: self.spare_sheets_timer,
            bleed: self.bleed_wc_ids.clone(),
        };
        let mut effects = Vec::with_capacity(self.tabs.len() + 8);
        self.append_activation_layout(&next.active, &mut next.split, environment, &mut effects);
        effects.push(TabActivationEffect::ActiveDocs(if target.kind == TabKind::Docs { target.view.clone() } else { None }));
        if let Some(view) = &target.view {
            if target.kind == TabKind::Sheets { effects.push(TabActivationEffect::ActiveSheets(view.clone())); }
            if target.kind == TabKind::Slides { effects.push(TabActivationEffect::ActiveSlides(view.clone())); }
        }
        effects.push(TabActivationEffect::Menu(if target.present || target.opener.is_some() { TabKind::Home } else { target.kind }));
        effects.push(TabActivationEffect::Changed);
        effects.push(TabActivationEffect::SplitChanged(next.split.clone()));
        Some(PreparedTabActivation { expected: self.activation_stamp(), next, effects })
    }

    /// The effect tail of ending an exclusive prompt, after its pure state restore.
    pub fn prepare_prompt_restore(&self, environment: ActivationEnvironment) -> PreparedTabActivation<V, B> {
        let mut prepared = self.prepare_current_layout(environment);
        self.append_refresh_focus(&self.active_id, environment, &mut prepared.effects);
        prepared.effects.push(TabActivationEffect::Changed);
        prepared.effects.push(TabActivationEffect::SplitChanged(prepared.next.split.clone()));
        prepared
    }

    /// Prepare the exact effects of the post-close strip reducer on its
    /// simulated manager. The returned transition is still checked at commit.
    pub fn prepare_generated_removal_effects(
        &self, removal: &TabRemovalState, environment: ActivationEnvironment,
    ) -> Result<PreparedTabActivation<V, B>, String> {
        let mut prepared = if let Some(target) = &removal.activate {
            self.prepare_loaded_activation(target, environment)?
        } else if removal.layout {
            let mut prepared = self.prepare_current_layout(environment);
            if let Some(target) = self.tab_by_id(&self.active_id) {
                prepared.effects.push(TabActivationEffect::ActiveDocs(
                    if target.kind == TabKind::Docs { target.view.clone() } else { None }));
                if let Some(view) = &target.view {
                    if target.kind == TabKind::Sheets { prepared.effects.push(TabActivationEffect::ActiveSheets(view.clone())); }
                    if target.kind == TabKind::Slides { prepared.effects.push(TabActivationEffect::ActiveSlides(view.clone())); }
                }
                prepared.effects.push(TabActivationEffect::Menu(if target.present || target.opener.is_some() {
                    TabKind::Home
                } else { target.kind }));
            }
            prepared.effects.push(TabActivationEffect::Changed);
            prepared
        } else {
            PreparedTabActivation {
                expected: self.activation_stamp(),
                next: ActivationState {
                    active: self.active_id.clone(), previous: self.previous_active_id.clone(), split: self.split.clone(),
                    spare: self.spare_sheets_view.clone(), spare_timer: self.spare_sheets_timer,
                    bleed: self.bleed_wc_ids.clone(),
                },
                effects: if removal.changed { vec![TabActivationEffect::Changed] } else { Vec::new() },
            }
        };
        if removal.split_changed { prepared.effects.push(TabActivationEffect::SplitChanged(prepared.next.split.clone())); }
        Ok(prepared)
    }

    fn append_refresh_focus(&self, active: &str, environment: ActivationEnvironment, effects: &mut Vec<TabActivationEffect<V, B>>) {
        if let Some(target) = self.tab_by_id(active) {
            effects.push(TabActivationEffect::ActiveDocs(
                if target.kind == TabKind::Docs { target.view.clone() } else { None }));
            if let Some(view) = &target.view {
                if target.kind == TabKind::Sheets { effects.push(TabActivationEffect::ActiveSheets(view.clone())); }
                if target.kind == TabKind::Slides { effects.push(TabActivationEffect::ActiveSlides(view.clone())); }
            }
            let menu = if target.present || target.opener.is_some() { TabKind::Home } else { target.kind };
            effects.push(TabActivationEffect::Menu(menu));
            if !environment.window_destroyed && environment.window_focused {
                effects.push(match &target.view {
                    Some(view) => TabActivationEffect::Focus(view.clone()),
                    None => TabActivationEffect::FocusShell,
                });
            }
        }
    }

    /// Only a short state reducer: no callbacks, layout, focus, menu, I/O or native calls.
    /// A rejected stale plan leaves all tab/spare/split fields unchanged.
    pub fn validate_loaded_activation(&self, prepared: &PreparedTabActivation<V, B>) -> Result<(), String> {
        let stamp = &prepared.expected;
        let tabs_match = self.tabs.len() == stamp.tabs.len() && self.tabs.iter().zip(&stamp.tabs)
            .all(|(tab, old)| tab.id == old.0 && tab.kind == old.1 && tab.view == old.2 && tab.binding == old.3 && tab.present == old.4 && tab.opener == old.5);
        if !tabs_match || self.active_id != stamp.active || self.previous_active_id != stamp.previous
            || self.split != stamp.split || self.spare_sheets_view != stamp.spare
            || self.spare_sheets_timer != stamp.spare_timer || self.html_full_screen_id != stamp.full_screen
            || self.bleed_wc_ids != stamp.bleed {
            return Err("the tab state changed before activation was admitted".into());
        }
        Ok(())
    }

    pub fn preview_loaded_activation(&self, prepared: &PreparedTabActivation<V, B>) -> Result<Self, String> {
        self.validate_loaded_activation(prepared)?;
        let mut preview = self.clone();
        preview.active_id = prepared.next.active.clone();
        preview.previous_active_id = prepared.next.previous.clone();
        preview.split = prepared.next.split.clone();
        preview.spare_sheets_view = prepared.next.spare.clone();
        preview.spare_sheets_timer = prepared.next.spare_timer;
        preview.bleed_wc_ids = prepared.next.bleed.clone();
        Ok(preview)
    }

    pub fn commit_loaded_activation(&mut self, prepared: PreparedTabActivation<V, B>) -> Result<Vec<TabActivationEffect<V, B>>, String> {
        self.validate_loaded_activation(&prepared)?;
        self.active_id = prepared.next.active;
        self.previous_active_id = prepared.next.previous;
        self.split = prepared.next.split;
        self.spare_sheets_view = prepared.next.spare;
        self.spare_sheets_timer = prepared.next.spare_timer;
        self.bleed_wc_ids = prepared.next.bleed;
        Ok(prepared.effects)
    }

    pub(super) fn append_activation_layout(&self, active: &str, split: &mut Option<SplitViewState>, environment: ActivationEnvironment, effects: &mut Vec<TabActivationEffect<V, B>>) {
        if environment.window_destroyed { return; }
        let (width, height) = environment.window_size;
        let strip = tab_strip_height(environment.ui_scale);
        let bounds = Rect { x: 0.0, y: strip, width, height: math_max(0.0, height - strip) };
        if !is_settled_rect(&bounds) { return; }
        if split.as_ref().is_some_and(|split|
            !self.tab_by_id(&split.a_tab_id).is_some_and(|tab| tab.view.is_some())
            || !self.tab_by_id(&split.b_tab_id).is_some_and(|tab| tab.view.is_some())) {
            *split = None;
        }
        let cover = self.html_full_screen_id.as_deref().or(if environment.target_bleeds { Some(active) } else { None });
        if let Some(cover) = cover {
            for tab in &self.tabs {
                if let Some(view) = &tab.view { effects.push(TabActivationEffect::Visible(view.clone(), tab.id == cover)); }
            }
            if let Some(view) = self.tab_by_id(cover).and_then(|tab| tab.view.as_ref()) {
                effects.push(TabActivationEffect::Bounds(view.clone(), Rect { x: 0.0, y: 0.0, width, height }));
            }
            return;
        }
        if let Some(split) = split.as_ref().filter(|split| active == split.a_tab_id || active == split.b_tab_id) {
            for tab in &self.tabs {
                if let Some(view) = &tab.view {
                    effects.push(TabActivationEffect::Visible(view.clone(), tab.id == split.a_tab_id || tab.id == split.b_tab_id));
                }
            }
            let panes = pane_rects(&bounds, split.orientation, split.ratio);
            for (id, bounds) in [(&split.a_tab_id, panes.a), (&split.b_tab_id, panes.b)] {
                if let Some(view) = self.tab_by_id(id).and_then(|tab| tab.view.as_ref()) {
                    effects.push(TabActivationEffect::Bounds(view.clone(), bounds));
                }
            }
            return;
        }
        for tab in &self.tabs {
            if let Some(view) = &tab.view { effects.push(TabActivationEffect::Visible(view.clone(), tab.id == active)); }
        }
        if let Some(view) = self.tab_by_id(active).and_then(|tab| tab.view.as_ref()) {
            effects.push(TabActivationEffect::Bounds(view.clone(), bounds));
        }
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;

    fn environment() -> ActivationEnvironment {
        ActivationEnvironment { window_destroyed: false, window_focused: true, window_size: (1000.0, 700.0),
            ui_scale: 1.0, no_spare_view: true, target_bleeds: false }
    }

    fn add(manager: &mut TabManager<u32, ()>, id: &str, view: u32, kind: TabKind) {
        manager.tabs.push(TabRecord { id: id.into(), kind, view: Some(view), binding: Some(()), title: id.into(),
            file_path: None, present: false, opener: None, lazy: None });
    }

    #[test]
    fn prepared_split_preserves_order_and_rejects_replaced_panes() {
        let mut manager = TabManager::new();
        add(&mut manager, "a", 11, TabKind::Docs);
        add(&mut manager, "b", 12, TabKind::Sheets);
        let prepared = manager.prepare_loaded_split("a", "b", SplitOrientation::Side, f64::NAN, Pane::B, environment()).unwrap().unwrap();
        assert!(manager.split.is_none());
        assert!(matches!(prepared.effects().last(), Some(TabActivationEffect::SplitChanged(Some(_)))));
        assert!(matches!(prepared.effects().get(prepared.effects().len() - 2), Some(TabActivationEffect::Changed)));
        manager.tabs.iter_mut().find(|tab| tab.id == "b").unwrap().view = Some(99);
        assert!(manager.commit_loaded_activation(prepared).is_err());
        assert!(manager.split.is_none());
        let next = manager.prepare_loaded_split("a", "b", SplitOrientation::Side, f64::NAN, Pane::B, environment()).unwrap().unwrap();
        let effects = manager.commit_loaded_activation(next).unwrap();
        assert_eq!(manager.active_id, "b");
        assert!(matches!(effects.last(), Some(TabActivationEffect::SplitChanged(Some(_)))));
        assert!(manager.prepare_loaded_split("a", "a", SplitOrientation::Side, 0.5, Pane::A, environment()).unwrap().is_none());
    }

    #[test]
    fn activation_retains_spare_binding_and_rejects_replacement() {
        let mut manager = TabManager::<u32, u64>::new();
        manager.spare_sheets_view = Some((17, 41));
        let prepared = manager.prepare_loaded_activation(HOME_ID, environment()).unwrap();
        let preview = manager.preview_loaded_activation(&prepared).unwrap();
        assert_eq!(manager.spare_sheets_view, Some((17, 41)));
        assert_eq!(preview.spare_sheets_view, None);
        assert!(prepared.effects().iter().any(|effect| matches!(effect, TabActivationEffect::RemoveChild(17, 41))));
        assert!(prepared.effects().iter().any(|effect| matches!(effect, TabActivationEffect::Close(17, 41))));
        manager.spare_sheets_view = Some((17, 42));
        assert!(manager.preview_loaded_activation(&prepared).is_err());
        assert!(manager.commit_loaded_activation(prepared).is_err());
        assert_eq!(manager.spare_sheets_view, Some((17, 42)));
    }

    #[test]
    fn saved_registration_rejects_stale_binding_before_actor_mutation() {
        let mut manager = TabManager::<u32, u64>::new();
        manager.tabs.push(TabRecord { id: "saved".into(), kind: TabKind::Docs, view: Some(17), binding: Some(41),
            title: "saved".into(), file_path: None, present: false, opener: None, lazy: None });
        let mut called = false;
        assert!(manager.replace_tab_binding_admitted("saved", &17, &40, 42, &mut || { called = true; Ok(()) }).is_err());
        assert!(!called);
        assert_eq!(manager.tab_by_id("saved").unwrap().binding, Some(41));
        assert!(manager.replace_tab_binding_admitted("saved", &17, &41, 42, &mut || Err("registration failed".into())).is_err());
        assert_eq!(manager.tab_by_id("saved").unwrap().binding, Some(41));
        manager.replace_tab_binding_admitted("saved", &17, &41, 42, &mut || Ok(())).unwrap();
        assert_eq!(manager.tab_by_id("saved").unwrap().binding, Some(42));
    }
}
