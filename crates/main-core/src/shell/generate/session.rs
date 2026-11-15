//! `apps/shell/src/main/generate/session.ts`: one open generate window's
//! state and actions, without Electron: the window module wires the
//! dependencies (file reads, the open tabs, the new tab) and pushes `state()`
//! to the renderer after every change.
//!
//! Rules held here:
//! - "make" re-reads every material first and goes ahead only with at least
//!   one readable material, none open with unsaved changes, and no more than
//!   the cap (8.3, 8.4). Unreadable ones are left out with their reasons.
//! - A document never saved cannot be material; it is listed with that
//!   reason, and once it is saved it becomes a normal row.
//! - "make" opens the new tab with the request queued for it, once, and
//!   closes the window. A second press while one is in flight does nothing.
//! - A conversion without AI (8.6) is offered only while exactly one material
//!   is listed and its type is the conversion's source.
//!
//! The TypeScript class holds its `deps`; here every method takes the host
//! ([`GenerateSessionHost`]) it runs against, so the window can lend its own
//! to a session it also owns. Every renderer, parser, conversion and tab-open
//! boundary yields an owned [`SessionNeed`]. The caller releases core state,
//! performs the effect, then resumes with a fresh [`OpenSnapshot`]. A move-only
//! [`SessionRun`] retains the exact row and action that requested the answer.

use jscompat::math_round;
use serde_json::{Value, json};
use std::sync::Arc;

use super::convert::{ConvertResult, ConvertedDocument};
use super::generate_api::{
    AiTargetKind, ConvertKind, ConvertProblem, Failed, GenerateMode, GenerateWindowState,
    MaterialProblem, MaterialState, MaterialView, Notices, SaveAction, StateLimits, ai_target_str,
    convert_kind_str, default_generate_prompt, parse_convert_kind,
};
use super::limits::{MAX_IMAGE_MATERIAL_BYTES, MAX_MATERIAL_BYTES, MAX_MATERIALS};
use super::materials::{
    MaterialStatus, material_key, material_max_bytes, summarize_materials, untitled_material,
};
use super::open_materials::{
    OpenSnapshot, SaveAnswer, SaveMaterialRun, SaveMaterialStep, SaveNeed, SaveOutcome,
};
use super::request::{
    PrepareRun, PrepareStep, Prepared, available_conversions, is_ai_target_kind,
    prepare_generate_request_begin,
};
use crate::packages::electron_utils::generate_handoff::{
    GenerateHandoff, GenerateHandoffTarget, Material,
};
use crate::packages::i18n::Lang;
use crate::shell::renderer_disposal::Failure;

/// where a row came from: a file, or an open document that was never saved
#[derive(Debug, Clone, PartialEq)]
pub enum MaterialRef {
    Path(String),
    Untitled { web_contents_id: f64, title: String },
}

/// `GenerateSessionDeps`. A call that rejects is a [`Failure`].
pub trait GenerateSessionHost {
    fn lang(&self) -> Lang;
    /// For a never-saved document: its path now, `None` while it is still untitled.
    /// (`deps.pathOfUntitled(..).catch(() => null)`: the open tabs are the snapshot's.)
    fn path_of_untitled(
        &mut self,
        snap: &OpenSnapshot<'_>,
        web_contents_id: f64,
    ) -> Result<Option<String>, Failure>;
    /// whether this path's open document can be saved in place, or only shown
    fn save_action_for(&mut self, snap: &OpenSnapshot<'_>, path: &str) -> SaveAction;
    /// save the open document behind `path`, up to the editor's save command
    /// (a rejection is [`SaveOutcome::Failed`])
    fn save_material_begin(
        &mut self,
        snap: &OpenSnapshot<'_>,
        path: &str,
    ) -> Result<SaveMaterialStep, Failure>;
    /// the editor answered a [`SaveNeed`]
    fn save_material_resume(&mut self, run: SaveMaterialRun, answer: SaveAnswer) -> SaveOutcome;
    /// bring a never-saved document's tab forward so the user can save it
    fn show_untitled(
        &mut self,
        snap: &OpenSnapshot<'_>,
        web_contents_id: f64,
    ) -> Result<Option<String>, Failure>;
    /// `deps.convert` and `deps.openConvertedTab` were both given: conversions are offered
    fn can_convert(&self) -> bool;
    /// `deps.redactionNotice` was given
    fn has_redaction_notice(&self) -> bool;
    fn push(&mut self, state: &GenerateWindowState) -> Result<(), Failure>;
    fn new_id(&mut self) -> String;
    fn now(&mut self) -> f64;
    /// a dirty flag can lag a save by a beat: when to look again (`None`: 600 ms)
    fn recheck_delay_ms(&self) -> Option<f64>;
    /// `MAX_OUTLINE_SLIDES` of `apps/shell/src/main/mcp/pptx-outline.ts`
    fn max_convert_slides(&self) -> usize;
}

struct Row {
    /// names this row where the TypeScript compares the object
    id: u64,
    key: String,
    mref: MaterialRef,
    /// `None` while being read
    status: Option<MaterialStatus>,
    save_action: Option<SaveAction>,
    /// a markdown file with withheld spans
    marked: bool,
}

fn key_of(mref: &MaterialRef) -> String {
    match mref {
        MaterialRef::Path(path) => material_key(path),
        MaterialRef::Untitled {
            web_contents_id, ..
        } => format!("untitled:{}", jscompat::number_to_string(*web_contents_id)),
    }
}

/// Owned effects. No session, host reference or core borrow crosses a yield.
#[derive(Debug)]
pub enum SessionNeed {
    Save(SaveNeed),
    Reveal(String),
    Sleep(f64),
    Inspect(String),
    Notice(String),
    Convert {
        kind: ConvertKind,
        path: String,
    },
    OpenTarget {
        handoff: GenerateHandoff,
        token: OpenCommitToken,
    },
    OpenConverted {
        document: ConvertedDocument,
        token: OpenCommitToken,
    },
}

#[derive(Debug)]
pub enum SessionAnswer {
    Save(SaveAnswer),
    Revealed(Result<(), Failure>),
    Slept,
    Inspected(Result<MaterialStatus, Failure>),
    Noticed(Result<bool, Failure>),
    Converted(Result<ConvertResult, Failure>),
    OpenFailed(Failure),
    Dialog(Result<super::generate_window::OpenDialogResult, Failure>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    Unit,
    Made(bool),
}

/// A move-only operation identity, not native authority. The host retains it
/// through the actual constructor, immutable seed commit and load outcome.
#[derive(Debug)]
pub struct OpenCommitToken {
    operation: Arc<()>,
}

#[derive(Debug)]
pub struct OpenCommitReceipt {
    operation: Arc<()>,
}

impl OpenCommitToken {
    /// The native/port wrapper must require its actual CommittedGeneratedTab
    /// evidence before consuming this token. A successful queue operation or
    /// Result<()> is not evidence of a committed target.
    pub fn commit(self) -> OpenCommitReceipt {
        OpenCommitReceipt {
            operation: self.operation,
        }
    }
}

#[derive(Debug)]
pub struct SessionRun {
    stage: RunStage,
}

#[derive(Debug)]
pub enum SessionStep {
    Done(Result<SessionOutcome, Failure>),
    Need(SessionNeed, SessionRun),
}

#[derive(Debug)]
enum AfterInspection {
    Unit,
    Saved { id: u64, result: SaveOutcome },
    Make { text: String },
}

#[derive(Debug)]
struct InspectionRun {
    rows: Vec<u64>,
    next: usize,
    failure: Option<Failure>,
    after: AfterInspection,
}

#[derive(Debug)]
enum InspectWait {
    Status {
        id: u64,
        path: String,
    },
    Notice {
        id: u64,
        status: MaterialStatus,
        save_action: Option<SaveAction>,
    },
}

#[derive(Debug)]
enum AfterPrepared {
    Ai { text: String },
    Convert { kind: ConvertKind },
}

#[derive(Debug)]
enum RunStage {
    Inspect {
        run: InspectionRun,
        waiting: InspectWait,
    },
    Saving {
        id: u64,
        row_key: String,
        run: SaveMaterialRun,
    },
    Revealing {
        saved: Option<(u64, String)>,
    },
    Waiting {
        id: u64,
    },
    Preparing {
        run: PrepareRun,
        after: AfterPrepared,
    },
    Converting {
        source: String,
    },
    Opening {
        operation: Arc<()>,
    },
}

pub struct GenerateSession {
    rows: Vec<Row>,
    next_row: u64,
    kind: AiTargetKind,
    making: bool,
    closed: bool,
    saving: Vec<String>,
    failure: Option<Failed>,
    convert_problem: Option<ConvertProblem>,
    in_flight: u32,
    mode: GenerateMode,
    convert_kind: Option<ConvertKind>,
}

impl GenerateSession {
    pub fn new(refs: &[MaterialRef], kind: Option<AiTargetKind>) -> Self {
        let mut session = GenerateSession {
            rows: Vec::new(),
            next_row: 0,
            kind: kind.unwrap_or(GenerateHandoffTarget::Slides),
            making: false,
            closed: false,
            saving: Vec::new(),
            failure: None,
            convert_problem: None,
            in_flight: 0,
            mode: GenerateMode::Ai,
            convert_kind: None,
        };
        session.append_refs(refs);
        session
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Start over without retaining a core borrow while rows are inspected.
    pub fn reset<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        refs: &[MaterialRef],
        kind: Option<AiTargetKind>,
    ) -> SessionStep {
        match self.reset_begin(host, refs, kind) {
            Ok(rows) => self.reset_finish(host, snap, rows),
            Err(error) => SessionStep::Done(Err(error)),
        }
    }

    pub fn reset_begin<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        refs: &[MaterialRef],
        kind: Option<AiTargetKind>,
    ) -> Result<Vec<u64>, Failure> {
        self.rows = Vec::new();
        if let Some(kind) = kind {
            self.kind = kind;
        }
        self.failure = None;
        self.convert_problem = None;
        self.mode = GenerateMode::Ai;
        self.convert_kind = None;
        self.append_refs(refs);
        self.inspect_begin(host)?;
        Ok(self.rows.iter().map(|row| row.id).collect())
    }

    pub fn reset_finish<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        rows: Vec<u64>,
    ) -> SessionStep {
        self.advance_inspection(
            host,
            snap,
            InspectionRun {
                rows,
                next: 0,
                failure: None,
                after: AfterInspection::Unit,
            },
        )
    }

    pub fn state<H: GenerateSessionHost>(&self, host: &H) -> GenerateWindowState {
        let statuses: Vec<MaterialStatus> =
            self.rows.iter().filter_map(|r| r.status.clone()).collect();
        let summary = summarize_materials(&statuses, self.kind);
        let checking = self.in_flight > 0 || self.rows.iter().any(|r| r.status.is_none());
        let materials: Vec<MaterialView> = self.rows.iter().map(|row| self.view_of(row)).collect();
        let conversions = self.conversions(host);
        let convert_kind = self.chosen_conversion(&conversions);
        // a conversion picked and then taken away by the list: "make" waits rather than run the AI
        let lost_conversion = self.mode == GenerateMode::Convert && convert_kind.is_none();
        GenerateWindowState {
            lang: host.lang(),
            kind: self.kind,
            mode: if convert_kind.is_some() {
                GenerateMode::Convert
            } else {
                GenerateMode::Ai
            },
            convert_kind,
            conversions,
            redaction_notice: convert_kind == Some(ConvertKind::MarkdownToMindmap)
                && self.listed_rows().first().is_some_and(|row| row.marked),
            materials,
            ready_count: summary.ready.len(),
            total_bytes: summary.total_bytes,
            // no AI, no plan usage: the AI notices do not apply to a conversion
            notices: if convert_kind.is_some() {
                Notices {
                    usage: false,
                    long_material: false,
                }
            } else {
                summary.notices
            },
            too_many: summary.too_many,
            can_make: summary.can_make
                && !checking
                && !self.making
                && self.saving.is_empty()
                && !lost_conversion,
            checking,
            making: self.making,
            saving: self.saving.clone(),
            failure: self.failure,
            convert_problem: if self.failure == Some(Failed::Convert) {
                self.convert_problem
            } else {
                None
            },
            limits: StateLimits {
                max_materials: MAX_MATERIALS,
                max_material_mb: math_round(MAX_MATERIAL_BYTES / 1024.0 / 1024.0),
                max_image_mb: math_round(MAX_IMAGE_MATERIAL_BYTES / 1024.0 / 1024.0),
                max_convert_slides: host.max_convert_slides(),
            },
        }
    }

    fn view_of(&self, row: &Row) -> MaterialView {
        let (name, path) = match &row.mref {
            MaterialRef::Path(path) => (
                row.status
                    .as_ref()
                    .map_or_else(|| path.clone(), |s| s.name.clone()),
                Some(path.clone()).filter(|p| !p.is_empty()),
            ),
            MaterialRef::Untitled { title, .. } => (title.clone(), None),
        };
        let Some(s) = &row.status else {
            return MaterialView {
                key: row.key.clone(),
                name,
                path,
                size_bytes: None,
                state: MaterialState::Checking,
                problem: None,
                limit_mb: None,
                save_action: None,
            };
        };
        let mut view = MaterialView {
            key: row.key.clone(),
            name: s.name.clone(),
            path,
            size_bytes: s.size_bytes,
            state: MaterialState::Ready,
            problem: None,
            limit_mb: None,
            save_action: None,
        };
        if let Some(problem) = s.problem {
            let limit = material_max_bytes(&s.ext);
            view.state = MaterialState::Excluded;
            view.problem = Some(problem);
            if problem == MaterialProblem::TooLarge {
                view.limit_mb = Some(math_round(limit / 1024.0 / 1024.0));
            }
            if problem == MaterialProblem::Untitled {
                view.save_action = Some(SaveAction::Show);
            }
            return view;
        }
        if s.unsaved {
            view.state = MaterialState::Unsaved;
            view.save_action = Some(row.save_action.unwrap_or(SaveAction::Show));
        }
        view
    }

    fn push<H: GenerateSessionHost>(&self, host: &mut H) -> Result<(), Failure> {
        if !self.closed {
            host.push(&self.state(host))?;
        }
        Ok(())
    }

    /// the rows that count as listed material: not left out (or still being read)
    fn listed_rows(&self) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| r.status.as_ref().is_none_or(|s| s.problem.is_none()))
            .collect()
    }

    /// 8.6: the conversions on offer: exactly one listed material, of a matching type
    fn conversions<H: GenerateSessionHost>(&self, host: &H) -> Vec<ConvertKind> {
        if !host.can_convert() {
            return Vec::new();
        }
        let listed = self.listed_rows();
        if listed.len() != 1 {
            return Vec::new();
        }
        match &listed[0].mref {
            MaterialRef::Path(path) => available_conversions(std::slice::from_ref(path)),
            MaterialRef::Untitled { .. } => Vec::new(),
        }
    }

    /// the chosen conversion while it is still on offer
    fn chosen_conversion(&self, conversions: &[ConvertKind]) -> Option<ConvertKind> {
        match self.convert_kind {
            Some(kind) if self.mode == GenerateMode::Convert && conversions.contains(&kind) => {
                Some(kind)
            }
            _ => None,
        }
    }

    fn append_refs(&mut self, refs: &[MaterialRef]) -> Vec<u64> {
        let mut added = Vec::new();
        for mref in refs {
            if matches!(mref, MaterialRef::Path(path) if path.is_empty()) {
                continue;
            }
            let key = key_of(mref);
            if self.rows.iter().any(|r| r.key == key) {
                continue;
            }
            let id = self.next_row;
            self.next_row += 1;
            self.rows.push(Row {
                id,
                key,
                mref: mref.clone(),
                status: None,
                save_action: None,
                marked: false,
            });
            added.push(id);
        }
        added
    }

    fn has_row(&self, id: u64) -> bool {
        self.rows.iter().any(|r| r.id == id)
    }

    fn row_mut(&mut self, id: u64) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }

    /// Resolve a row's current path on core. Saved untitled rows retain their
    /// identity; a removed row cannot be recreated by a late worker answer.
    fn inspection_path<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        id: u64,
    ) -> Result<Option<String>, Failure> {
        let untitled = self
            .rows
            .iter()
            .find(|row| row.id == id)
            .and_then(|row| match &row.mref {
                MaterialRef::Untitled {
                    web_contents_id,
                    title,
                } => Some((*web_contents_id, title.clone())),
                MaterialRef::Path(_) => None,
            });
        if let Some((web_contents_id, title)) = untitled {
            match host
                .path_of_untitled(snap, web_contents_id)?
                .filter(|path| !path.is_empty())
            {
                Some(saved) => {
                    let key = material_key(&saved);
                    if self.rows.iter().any(|row| row.id != id && row.key == key) {
                        self.rows.retain(|row| row.id != id);
                        return Ok(None);
                    }
                    if let Some(row) = self.row_mut(id) {
                        row.mref = MaterialRef::Path(saved);
                        row.key = key;
                    }
                }
                None => {
                    if let Some(row) = self.row_mut(id) {
                        row.status = Some(untitled_material(web_contents_id, &title));
                    }
                    return Ok(None);
                }
            }
        }
        Ok(
            match self
                .rows
                .iter()
                .find(|row| row.id == id)
                .map(|row| &row.mref)
            {
                Some(MaterialRef::Path(path)) => Some(path.clone()),
                _ => None,
            },
        )
    }

    fn apply_inspection(
        &mut self,
        id: u64,
        mut status: MaterialStatus,
        save_action: Option<SaveAction>,
        marked: bool,
    ) {
        if let Some(row) = self.row_mut(id) {
            status.key = row.key.clone();
            row.status = Some(status);
            row.save_action = save_action;
            row.marked = marked;
        }
    }

    fn inspect_begin<H: GenerateSessionHost>(&mut self, host: &mut H) -> Result<(), Failure> {
        self.in_flight += 1;
        if let Err(error) = self.push(host) {
            self.in_flight -= 1;
            return Err(error);
        }
        Ok(())
    }

    fn inspect_rows<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        rows: Vec<u64>,
        after: AfterInspection,
    ) -> SessionStep {
        if let Err(error) = self.inspect_begin(host) {
            if matches!(after, AfterInspection::Make { .. }) {
                self.making = false;
            }
            return SessionStep::Done(Err(error));
        }
        self.advance_inspection(
            host,
            snap,
            InspectionRun {
                rows,
                next: 0,
                failure: None,
                after,
            },
        )
    }

    fn advance_inspection<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        mut run: InspectionRun,
    ) -> SessionStep {
        while let Some(&id) = run.rows.get(run.next) {
            run.next += 1;
            match self.inspection_path(host, snap, id) {
                Ok(Some(path)) => {
                    return SessionStep::Need(
                        SessionNeed::Inspect(path.clone()),
                        SessionRun {
                            stage: RunStage::Inspect {
                                run,
                                waiting: InspectWait::Status { id, path },
                            },
                        },
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    if run.failure.is_none() {
                        run.failure = Some(error);
                    }
                }
            }
        }
        self.in_flight -= 1;
        if let Err(error) = self.push(host) {
            if matches!(run.after, AfterInspection::Make { .. }) {
                self.making = false;
            }
            return SessionStep::Done(Err(error));
        }
        let result = run.failure.map_or(Ok(()), Err);
        match run.after {
            AfterInspection::Unit => SessionStep::Done(result.map(|()| SessionOutcome::Unit)),
            AfterInspection::Make { text } => match result {
                Ok(()) => self.prepare_make(host, text),
                Err(_) => self.failed_make(host),
            },
            AfterInspection::Saved { id, result: saved } => {
                if let Err(error) = result {
                    return SessionStep::Done(Err(error));
                }
                let still_unsaved = self
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .and_then(|row| row.status.as_ref())
                    .is_some_and(|status| status.unsaved);
                if saved == SaveOutcome::Saved && still_unsaved {
                    SessionStep::Need(
                        SessionNeed::Sleep(host.recheck_delay_ms().unwrap_or(600.0)),
                        SessionRun {
                            stage: RunStage::Waiting { id },
                        },
                    )
                } else {
                    SessionStep::Done(Ok(SessionOutcome::Unit))
                }
            }
        }
    }

    pub fn refresh<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
    ) -> SessionStep {
        let rows = self.rows.iter().map(|row| row.id).collect();
        self.inspect_rows(host, snap, rows, AfterInspection::Unit)
    }

    pub fn set_kind<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        kind: Option<&Value>,
    ) -> Result<(), Failure> {
        if !is_ai_target_kind(kind) || self.making {
            return Ok(());
        }
        let text = kind.and_then(Value::as_str).unwrap_or_default();
        if let Some(kind) = GenerateHandoffTarget::parse(text) {
            self.kind = kind;
        }
        self.mode = GenerateMode::Ai;
        self.failure = None;
        self.push(host)
    }

    /// 8.6: choose a conversion on offer, or null for back to the AI kinds
    pub fn set_convert<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        kind: Option<&Value>,
    ) -> Result<(), Failure> {
        if self.making {
            return Ok(());
        }
        if matches!(kind, Some(Value::Null)) {
            self.mode = GenerateMode::Ai;
        } else {
            let parsed = kind.and_then(Value::as_str).and_then(parse_convert_kind);
            let Some(parsed) = parsed.filter(|k| self.conversions(&*host).contains(k)) else {
                return Ok(());
            };
            self.mode = GenerateMode::Convert;
            self.convert_kind = Some(parsed);
        }
        self.failure = None;
        self.convert_problem = None;
        self.push(host)
    }

    pub fn add_paths<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        paths: &[Value],
    ) -> SessionStep {
        if self.making {
            return SessionStep::Done(Ok(SessionOutcome::Unit));
        }
        self.failure = None;
        let refs: Vec<MaterialRef> = paths
            .iter()
            .filter_map(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(|p| MaterialRef::Path(p.to_owned()))
            .collect();
        let added = self.append_refs(&refs);
        if !added.is_empty() {
            self.inspect_rows(host, snap, added, AfterInspection::Unit)
        } else {
            SessionStep::Done(self.push(host).map(|()| SessionOutcome::Unit))
        }
    }

    pub fn remove<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        key: Option<&Value>,
    ) -> Result<(), Failure> {
        let Some(key) = key.and_then(Value::as_str) else {
            return Ok(());
        };
        if self.making {
            return Ok(());
        }
        self.rows.retain(|r| r.key != key);
        self.failure = None;
        self.push(host)
    }

    pub fn save_begin<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        key: Option<&Value>,
    ) -> SessionStep {
        let found = key
            .and_then(Value::as_str)
            .and_then(|key| self.rows.iter().find(|row| row.key == key));
        let Some(row) = found else {
            return SessionStep::Done(Ok(SessionOutcome::Unit));
        };
        if self.making || self.saving.contains(&row.key) {
            return SessionStep::Done(Ok(SessionOutcome::Unit));
        }
        let (id, row_key, mref) = (row.id, row.key.clone(), row.mref.clone());
        self.failure = None;
        let path = match mref {
            MaterialRef::Untitled {
                web_contents_id, ..
            } => {
                return match host.show_untitled(snap, web_contents_id) {
                    Ok(Some(tab)) => SessionStep::Need(
                        SessionNeed::Reveal(tab),
                        SessionRun {
                            stage: RunStage::Revealing { saved: None },
                        },
                    ),
                    Ok(None) => SessionStep::Done(self.push(host).map(|()| SessionOutcome::Unit)),
                    Err(error) => SessionStep::Done(Err(error)),
                };
            }
            MaterialRef::Path(path) => path,
        };
        self.saving.push(row_key.clone());
        if let Err(error) = self.push(host) {
            self.saving.retain(|key| key != &row_key);
            return SessionStep::Done(Err(error));
        }
        match host.save_material_begin(snap, &path) {
            Err(error) => {
                self.saving.retain(|key| key != &row_key);
                SessionStep::Done(Err(error))
            }
            Ok(SaveMaterialStep::Done(result)) => self.after_save(host, snap, id, row_key, result),
            Ok(SaveMaterialStep::Reveal(tab)) => SessionStep::Need(
                SessionNeed::Reveal(tab),
                SessionRun {
                    stage: RunStage::Revealing {
                        saved: Some((id, row_key)),
                    },
                },
            ),
            Ok(SaveMaterialStep::Need(need, run)) => SessionStep::Need(
                SessionNeed::Save(need),
                SessionRun {
                    stage: RunStage::Saving { id, row_key, run },
                },
            ),
        }
    }

    fn after_save<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        id: u64,
        row_key: String,
        result: SaveOutcome,
    ) -> SessionStep {
        self.saving.retain(|key| key != &row_key);
        if result == SaveOutcome::Failed {
            self.failure = Some(Failed::Save);
        }
        self.inspect_rows(host, snap, vec![id], AfterInspection::Saved { id, result })
    }

    /// Resume only the action named by this move-only token.
    pub fn resume<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        run: SessionRun,
        answer: SessionAnswer,
    ) -> SessionStep {
        match (run.stage, answer) {
            (RunStage::Revealing { saved }, SessionAnswer::Revealed(answer)) => {
                match (saved, answer) {
                    (Some((id, row_key)), Ok(())) => {
                        self.after_save(host, snap, id, row_key, SaveOutcome::Shown)
                    }
                    (Some((_, row_key)), Err(error)) => {
                        self.saving.retain(|key| key != &row_key);
                        SessionStep::Done(Err(error))
                    }
                    (None, Ok(())) => {
                        SessionStep::Done(self.push(host).map(|()| SessionOutcome::Unit))
                    }
                    (None, Err(error)) => SessionStep::Done(Err(error)),
                }
            }
            (RunStage::Saving { id, row_key, run }, SessionAnswer::Save(answer)) => {
                let result = host.save_material_resume(run, answer);
                self.after_save(host, snap, id, row_key, result)
            }
            (RunStage::Waiting { id }, SessionAnswer::Slept) => {
                if self.has_row(id) {
                    self.inspect_rows(host, snap, vec![id], AfterInspection::Unit)
                } else {
                    SessionStep::Done(Ok(SessionOutcome::Unit))
                }
            }
            (
                RunStage::Inspect {
                    mut run,
                    waiting: InspectWait::Status { id, path },
                },
                SessionAnswer::Inspected(answer),
            ) => {
                match answer {
                    Err(error) => {
                        if run.failure.is_none() {
                            run.failure = Some(error);
                        }
                    }
                    Ok(status) => {
                        let save_action = status.unsaved.then(|| host.save_action_for(snap, &path));
                        if host.has_redaction_notice()
                            && status.problem.is_none()
                            && !status.unsaved
                            && available_conversions(std::slice::from_ref(&path))
                                .contains(&ConvertKind::MarkdownToMindmap)
                        {
                            return SessionStep::Need(
                                SessionNeed::Notice(path),
                                SessionRun {
                                    stage: RunStage::Inspect {
                                        run,
                                        waiting: InspectWait::Notice {
                                            id,
                                            status,
                                            save_action,
                                        },
                                    },
                                },
                            );
                        }
                        self.apply_inspection(id, status, save_action, false);
                    }
                }
                self.advance_inspection(host, snap, run)
            }
            (
                RunStage::Inspect {
                    run,
                    waiting:
                        InspectWait::Notice {
                            id,
                            status,
                            save_action,
                        },
                },
                SessionAnswer::Noticed(answer),
            ) => {
                self.apply_inspection(id, status, save_action, answer.unwrap_or(false));
                self.advance_inspection(host, snap, run)
            }
            (RunStage::Preparing { run, after }, SessionAnswer::Inspected(answer)) => {
                self.prepared_step(host, run.resume(answer), after)
            }
            (RunStage::Converting { source }, SessionAnswer::Converted(answer)) => match answer {
                Err(_) => self.failed_make(host),
                Ok(Err(problem)) => {
                    self.failure = Some(Failed::Convert);
                    self.convert_problem = Some(problem);
                    self.stop_making(host, false)
                }
                Ok(Ok(content)) => {
                    let document = ConvertedDocument {
                        id: host.new_id(),
                        content,
                        sources: vec![source],
                        created_at: host.now(),
                    };
                    let token = OpenCommitToken {
                        operation: Arc::new(()),
                    };
                    let operation = token.operation.clone();
                    SessionStep::Need(
                        SessionNeed::OpenConverted { document, token },
                        SessionRun {
                            stage: RunStage::Opening { operation },
                        },
                    )
                }
            },
            (RunStage::Opening { .. }, SessionAnswer::OpenFailed(_)) => self.failed_make(host),
            (stage, _) => {
                self.discard_run(SessionRun { stage });
                SessionStep::Done(Err(Failure(
                    "unexpected generate continuation answer".into(),
                )))
            }
        }
    }

    /// This terminal path cannot inspect, convert, open again, or invoke a host.
    pub fn complete_committed_open(
        &mut self,
        run: SessionRun,
        receipt: OpenCommitReceipt,
    ) -> Result<SessionOutcome, Failure> {
        match run.stage {
            RunStage::Opening { operation } if Arc::ptr_eq(&operation, &receipt.operation) => {
                self.closed = true;
                Ok(SessionOutcome::Made(true))
            }
            stage => {
                self.discard_run(SessionRun { stage });
                Err(Failure(
                    "committed generate operation does not match its continuation".into(),
                ))
            }
        }
    }

    /// Cancellation cleanup has no host effects and cannot push stale state.
    pub fn discard_run(&mut self, run: SessionRun) {
        match run.stage {
            RunStage::Inspect { run, .. } => {
                self.in_flight -= 1;
                if matches!(run.after, AfterInspection::Make { .. }) {
                    self.making = false;
                }
            }
            RunStage::Saving { row_key, .. } => self.saving.retain(|key| key != &row_key),
            RunStage::Revealing {
                saved: Some((_, row_key)),
            } => self.saving.retain(|key| key != &row_key),
            RunStage::Revealing { saved: None } => {}
            RunStage::Waiting { .. } => {}
            RunStage::Preparing { .. } | RunStage::Converting { .. } | RunStage::Opening { .. } => {
                self.making = false
            }
        }
    }

    pub fn make<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        text: Option<&Value>,
    ) -> SessionStep {
        let Some(text) = text.and_then(Value::as_str) else {
            return SessionStep::Done(Ok(SessionOutcome::Made(false)));
        };
        if self.closed || self.making {
            return SessionStep::Done(Ok(SessionOutcome::Made(false)));
        }
        self.making = true;
        self.failure = None;
        if let Err(error) = self.push(host) {
            self.making = false;
            return SessionStep::Done(Err(error));
        }
        let rows = self.rows.iter().map(|row| row.id).collect();
        self.inspect_rows(
            host,
            snap,
            rows,
            AfterInspection::Make {
                text: text.to_owned(),
            },
        )
    }

    fn prepare_make<H: GenerateSessionHost>(&mut self, host: &mut H, text: String) -> SessionStep {
        let (raw, after) = if self.mode == GenerateMode::Convert {
            let Some(kind) = self.chosen_conversion(&self.conversions(host)) else {
                return self.stop_making(host, false);
            };
            let path = match self.listed_rows().first().map(|row| &row.mref) {
                Some(MaterialRef::Path(path)) if host.can_convert() => path.clone(),
                _ => return self.stop_making(host, false),
            };
            (
                json!({ "mode": "convert", "kind": convert_kind_str(kind), "materials": [path], "text": "" }),
                AfterPrepared::Convert { kind },
            )
        } else {
            let materials: Vec<Value> = self
                .rows
                .iter()
                .filter(|row| {
                    row.status
                        .as_ref()
                        .is_some_and(|status| status.problem.is_none())
                })
                .filter_map(|row| match &row.mref {
                    MaterialRef::Path(path) => Some(Value::from(path.as_str())),
                    MaterialRef::Untitled { .. } => None,
                })
                .collect();
            (
                json!({ "mode": "ai", "kind": ai_target_str(self.kind), "materials": materials, "text": text.as_str() }),
                AfterPrepared::Ai { text },
            )
        };
        self.prepared_step(host, prepare_generate_request_begin(&raw), after)
    }

    fn prepared_step<H: GenerateSessionHost>(
        &mut self,
        host: &mut H,
        step: PrepareStep,
        after: AfterPrepared,
    ) -> SessionStep {
        match step {
            PrepareStep::Inspect { path, run } => SessionStep::Need(
                SessionNeed::Inspect(path),
                SessionRun {
                    stage: RunStage::Preparing { run, after },
                },
            ),
            PrepareStep::Done(Err(_)) => self.failed_make(host),
            PrepareStep::Done(Ok(Prepared::Refused { .. })) => self.stop_making(host, false),
            PrepareStep::Done(Ok(Prepared::Ready { request, ready, .. })) => match after {
                AfterPrepared::Convert { kind } => {
                    let source = request
                        .materials
                        .into_iter()
                        .next()
                        .expect("a ready conversion has one material");
                    SessionStep::Need(
                        SessionNeed::Convert {
                            kind,
                            path: source.clone(),
                        },
                        SessionRun {
                            stage: RunStage::Converting { source },
                        },
                    )
                }
                AfterPrepared::Ai { text } => {
                    let trimmed = jscompat::trim(&text);
                    let id = host.new_id();
                    let lang = host.lang();
                    let created_at = host.now();
                    let handoff = GenerateHandoff {
                        id,
                        mode: "ai".into(),
                        target: self.kind,
                        materials: ready
                            .into_iter()
                            .map(|status| Material {
                                path: status.path.unwrap_or_default(),
                                name: status.name,
                            })
                            .collect(),
                        text: if trimmed.is_empty() {
                            default_generate_prompt(lang, self.kind).to_owned()
                        } else {
                            trimmed.to_owned()
                        },
                        default_text: trimmed.is_empty(),
                        created_at,
                    };
                    let token = OpenCommitToken {
                        operation: Arc::new(()),
                    };
                    let operation = token.operation.clone();
                    SessionStep::Need(
                        SessionNeed::OpenTarget { handoff, token },
                        SessionRun {
                            stage: RunStage::Opening { operation },
                        },
                    )
                }
            },
        }
    }

    fn failed_make<H: GenerateSessionHost>(&mut self, host: &mut H) -> SessionStep {
        self.failure = Some(Failed::Make);
        self.stop_making(host, false)
    }

    fn stop_making<H: GenerateSessionHost>(&mut self, host: &mut H, result: bool) -> SessionStep {
        self.making = false;
        SessionStep::Done(self.push(host).map(|()| SessionOutcome::Made(result)))
    }
}
