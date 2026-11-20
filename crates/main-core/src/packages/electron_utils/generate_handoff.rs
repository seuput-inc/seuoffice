//! `packages/electron-utils/src/generate-handoff.ts`.
//!
//! The generate hand-off: "make" leaves the request for the new tab, and the
//! editor in that tab takes it, once. The TypeScript keeps its store in a
//! `Symbol.for` key of `globalThis`, shared by every bundled copy of the
//! module; here the owner keeps one [`HandoffStore`] for the process.
//! `Date.now()` is the `now_ms` of the call, `process.platform` the `win32`
//! flag of the store, `path.resolve` the host's.

use jscompat::node_path::host::resolve;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::Cell;
use std::rc::Rc;

pub use super::handoff_editors::{
    HANDOFF_EDITORS, HandoffEditor, handoff_editor_takes_attachments, is_handoff_editor,
};

/// the editor of each document extension a template may have (spec 5.7)
const EDITOR_BY_EXTENSION: [(&str, HandoffEditor); 17] = [
    ("docx", HandoffEditor::Docs),
    ("odt", HandoffEditor::Docs),
    ("xlsx", HandoffEditor::Sheets),
    ("xlsm", HandoffEditor::Sheets),
    ("xls", HandoffEditor::Sheets),
    ("csv", HandoffEditor::Sheets),
    ("tsv", HandoffEditor::Sheets),
    ("pptx", HandoffEditor::Slides),
    ("pdf", HandoffEditor::Pdf),
    ("md", HandoffEditor::Markdown),
    ("markdown", HandoffEditor::Markdown),
    ("txt", HandoffEditor::Markdown),
    ("html", HandoffEditor::Html),
    ("htm", HandoffEditor::Html),
    ("xmind", HandoffEditor::Mindmap),
    ("hwp", HandoffEditor::Hwp),
    ("hwpx", HandoffEditor::Hwp),
];

/// the editor that opens a file of this name (a path is fine), or `None`
pub fn handoff_editor_for_file(file_name: &str) -> Option<HandoffEditor> {
    // `fileName.split(/[\\/]/).pop() ?? ''`
    let base = file_name.rsplit(['\\', '/']).next().unwrap_or("");
    // `lastIndexOf('.')` in UTF-16 units: `dot <= 0 || dot === base.length - 1`
    let dot = base.rfind('.')?;
    if dot == 0 || dot == base.len() - 1 {
        return None;
    }
    let extension = base[dot + 1..].to_lowercase();
    EDITOR_BY_EXTENSION
        .iter()
        .find(|(name, _)| *name == extension)
        .map(|(_, editor)| *editor)
}

/// the editors the generate window's "make" opens
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerateHandoffTarget {
    #[serde(rename = "slides")]
    Slides,
    #[serde(rename = "html")]
    Html,
    #[serde(rename = "mindmap")]
    Mindmap,
}

impl GenerateHandoffTarget {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "slides" => Some(Self::Slides),
            "html" => Some(Self::Html),
            "mindmap" => Some(Self::Mindmap),
            _ => None,
        }
    }
}

/// the editors a conversion's result opens in, as a new unsaved tab
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerateConvertTarget {
    #[serde(rename = "markdown")]
    Markdown,
    #[serde(rename = "slides")]
    Slides,
    #[serde(rename = "mindmap")]
    Mindmap,
}

/// the conversions without AI (8.6)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerateConvertKind {
    #[serde(rename = "mindmap-to-markdown")]
    MindmapToMarkdown,
    #[serde(rename = "mindmap-to-slides")]
    MindmapToSlides,
    #[serde(rename = "markdown-to-mindmap")]
    MarkdownToMindmap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Material {
    pub path: String,
    pub name: String,
}

/// What a new tab takes, once (8.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateHandoff {
    /// unique per press of "make"
    pub id: String,
    pub mode: String,
    pub target: GenerateHandoffTarget,
    /// the readable materials, in the order the window listed them
    pub materials: Vec<Material>,
    /// the user's words, or the kind's default request when they wrote none
    pub text: String,
    /// true when `text` is the default request (the box was left empty)
    #[serde(rename = "defaultText")]
    pub default_text: bool,
    /// when "make" was pressed, ms since epoch
    #[serde(rename = "createdAt")]
    pub created_at: f64,
}

/// one "a run started with handed-over material" record
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GenerateRunStarted {
    #[serde(rename = "tabId", skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(rename = "webContentsId")]
    pub web_contents_id: f64,
    pub target: GenerateHandoffTarget,
    pub mode: String,
    /// the handed-over material paths that were attached to the run
    pub sources: Vec<String>,
    /// every material the hand-off carried, attached or not
    #[serde(rename = "handedOver")]
    pub handed_over: Vec<String>,
    #[serde(rename = "handoffId")]
    pub handoff_id: String,
    /// ms since epoch
    pub at: f64,
}

/// one "a conversion without AI opened this new tab" record (8.6, spec 9)
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GenerateConverted {
    #[serde(rename = "tabId", skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(rename = "webContentsId")]
    pub web_contents_id: f64,
    pub target: GenerateConvertTarget,
    pub kind: GenerateConvertKind,
    pub mode: String,
    pub sources: Vec<String>,
    #[serde(rename = "conversionId")]
    pub conversion_id: String,
    pub at: f64,
}

/// a skill chip as it travels with a hand-off
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffSkillChip {
    pub id: String,
    pub source: String,
    pub title: String,
}

/// What a new tab's AI panel takes, once (spec 5.6, 5.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillHandoff {
    pub id: String,
    pub mode: String,
    pub target: HandoffEditorName,
    pub text: String,
    pub chips: Vec<HandoffSkillChip>,
    /// the template preview (`SkillHandoffPreview`), plain data this module does not read
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Value>,
    /// the asking tab's saved document (its path)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<Vec<String>>,
    #[serde(rename = "createdAt")]
    pub created_at: f64,
}

/// A [`HandoffEditor`] as the JSON of a hand-off names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandoffEditorName(pub HandoffEditor);

impl Serialize for HandoffEditorName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for HandoffEditorName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        HandoffEditor::parse(&text)
            .map(HandoffEditorName)
            .ok_or_else(|| serde::de::Error::custom("not a hand-off editor"))
    }
}

/// `new Map<number, V>()`: insertion order, a key set again keeps its place
/// (keys are `SameValueZero`).
#[derive(Debug, Clone)]
struct IdMap<V>(Vec<(f64, V)>);

impl<V> IdMap<V> {
    fn new() -> Self {
        IdMap(Vec::new())
    }

    fn at(&self, id: f64) -> Option<usize> {
        self.0
            .iter()
            .position(|(key, _)| *key == id || (key.is_nan() && id.is_nan()))
    }

    fn get(&self, id: f64) -> Option<&V> {
        self.at(id).map(|i| &self.0[i].1)
    }

    fn set(&mut self, id: f64, value: V) {
        match self.at(id) {
            Some(i) => self.0[i].1 = value,
            None => self.0.push((id, value)),
        }
    }

    fn delete(&mut self, id: f64) -> Option<V> {
        self.at(id).map(|i| self.0.remove(i).1)
    }
}

/// keep the in-memory event list bounded across a long session
const MAX_RUN_EVENTS: usize = 200;

/// `(webContentsId) => string | undefined`; `Err` is a resolver that throws
pub type TabIdResolver = dyn Fn(f64) -> Result<Option<String>, String>;

/// the part of a webContents this module needs (Electron's satisfies it)
pub trait HandoffWebContents {
    fn id(&self) -> f64;
    fn is_destroyed(&self) -> bool;
    /// `contents.once('destroyed', ...)`: when the event fires, the adapter
    /// calls [`HandoffStore::drop_generate_handoff`] with [`Self::id`]
    fn once_destroyed(&mut self);
}

/// one entry of `queuedSkillHandoffs()`
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueuedSkillHandoff {
    #[serde(rename = "webContentsId")]
    pub web_contents_id: f64,
    pub target: HandoffEditorName,
    pub id: String,
}

/// one entry of `queuedGenerateHandoffs()`
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueuedGenerateHandoff {
    #[serde(rename = "webContentsId")]
    pub web_contents_id: f64,
    pub target: GenerateHandoffTarget,
    pub id: String,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueStatus {
    Queued,
    Consumed,
    RolledBack,
    RetiredConsumed,
}
struct QueueIdentity {
    status: Cell<QueueStatus>,
    owned: bool,
}

/// Identity and displaced state for a single admitted handoff insertion.
pub struct GenerateQueueReceipt {
    id: f64,
    token: Rc<QueueIdentity>,
    previous_token: Option<Rc<QueueIdentity>>,
    previous_queued: Option<(usize, GenerateHandoff)>,
    previous_taken: Option<(usize, GenerateHandoff)>,
}
impl GenerateQueueReceipt {
    pub fn was_consumed(&self) -> bool {
        matches!(
            self.token.status.get(),
            QueueStatus::Consumed | QueueStatus::RetiredConsumed
        )
    }
}

/// Identity for a single conversion event, including events it displaced.
pub struct ConvertedEventReceipt {
    token: Rc<()>,
    evicted: Vec<(Rc<()>, GenerateConverted)>,
    done: bool,
}
pub struct PreparedConvertedEvent {
    event: GenerateConverted,
    returned: GenerateConverted,
    token: Rc<()>,
    evicted: Vec<(Rc<()>, GenerateConverted)>,
}

pub struct PreparedGenerateQueue {
    id: f64,
    handoff: GenerateHandoff,
    token: Rc<QueueIdentity>,
}
pub struct PreparedGenerateRun {
    id: f64,
    token: Rc<QueueIdentity>,
    result: Result<(GenerateRunStarted, GenerateRunStarted), String>,
}

/// `HandoffStore`
pub struct HandoffStore {
    /// `process.platform === 'win32'`: path keys are compared in lower case
    pub win32: bool,
    queued: IdMap<GenerateHandoff>,
    skill_queued: IdMap<SkillHandoff>,
    /// taken but not yet reported as run: what a run-started report is checked against
    taken: IdMap<GenerateHandoff>,
    queue_tokens: IdMap<Rc<QueueIdentity>>,
    runs: Vec<GenerateRunStarted>,
    converted: Vec<(Rc<()>, GenerateConverted)>,
    tab_id_of: Option<Box<TabIdResolver>>,
}

impl HandoffStore {
    pub fn new(win32: bool) -> Self {
        HandoffStore {
            win32,
            queued: IdMap::new(),
            skill_queued: IdMap::new(),
            taken: IdMap::new(),
            queue_tokens: IdMap::new(),
            runs: Vec::new(),
            converted: Vec::new(),
            tab_id_of: None,
        }
    }

    fn path_key(&self, path: &str) -> String {
        let resolved = resolve(&[path]);
        if self.win32 {
            resolved.to_lowercase()
        } else {
            resolved
        }
    }

    /// queue a request for a webContents; replaces anything still queued there
    pub fn queue_generate_handoff(&mut self, web_contents_id: f64, handoff: GenerateHandoff) {
        self.taken.delete(web_contents_id);
        self.queued.set(web_contents_id, handoff);
        self.queue_tokens.set(
            web_contents_id,
            Rc::new(QueueIdentity {
                status: Cell::new(QueueStatus::Queued),
                owned: false,
            }),
        );
    }

    pub fn queue_generate_handoff_reversible(
        &mut self,
        id: f64,
        handoff: GenerateHandoff,
    ) -> GenerateQueueReceipt {
        let token = Rc::new(QueueIdentity {
            status: Cell::new(QueueStatus::Queued),
            owned: true,
        });
        self.commit_generate_queue(PreparedGenerateQueue { id, handoff, token })
    }

    pub fn prepare_generate_queue(
        &mut self,
        id: f64,
        handoff: GenerateHandoff,
    ) -> Result<PreparedGenerateQueue, String> {
        self.queued
            .0
            .try_reserve(1)
            .map_err(|error| error.to_string())?;
        self.queue_tokens
            .0
            .try_reserve(1)
            .map_err(|error| error.to_string())?;
        Ok(PreparedGenerateQueue {
            id,
            handoff,
            token: Rc::new(QueueIdentity {
                status: Cell::new(QueueStatus::Queued),
                owned: true,
            }),
        })
    }

    pub fn can_commit_generate_queue(&self, plan: &PreparedGenerateQueue) -> bool {
        (self.queued.at(plan.id).is_some() || self.queued.0.len() < self.queued.0.capacity())
            && (self.queue_tokens.at(plan.id).is_some()
                || self.queue_tokens.0.len() < self.queue_tokens.0.capacity())
    }

    pub fn commit_generate_queue(&mut self, plan: PreparedGenerateQueue) -> GenerateQueueReceipt {
        let PreparedGenerateQueue { id, handoff, token } = plan;
        let previous_queued = self
            .queued
            .at(id)
            .map(|index| (index, self.queued.0.remove(index).1));
        let previous_taken = self
            .taken
            .at(id)
            .map(|index| (index, self.taken.0.remove(index).1));
        let previous_token = self.queue_tokens.delete(id);
        if let Some((index, _)) = &previous_queued {
            self.queued
                .0
                .insert((*index).min(self.queued.0.len()), (id, handoff));
        } else {
            self.queued.set(id, handoff);
        }
        self.queue_tokens.set(id, token.clone());
        GenerateQueueReceipt {
            id,
            token,
            previous_token,
            previous_queued,
            previous_taken,
        }
    }

    pub fn rollback_generate_handoff(
        &mut self,
        receipt: &mut GenerateQueueReceipt,
    ) -> Result<(), String> {
        match receipt.token.status.get() {
            QueueStatus::Consumed | QueueStatus::RolledBack | QueueStatus::RetiredConsumed => {
                return Ok(());
            }
            QueueStatus::Queued => {}
        }
        if !self
            .queue_tokens
            .get(receipt.id)
            .is_some_and(|token| Rc::ptr_eq(token, &receipt.token))
        {
            return Err("The generate handoff was replaced before rollback".into());
        }
        self.queued.delete(receipt.id);
        self.taken.delete(receipt.id);
        self.queue_tokens.delete(receipt.id);
        let restore = receipt.previous_token.as_ref().is_none_or(|token| {
            !matches!(
                token.status.get(),
                QueueStatus::RolledBack | QueueStatus::RetiredConsumed
            )
        });
        if restore {
            if let Some((index, handoff)) = receipt.previous_queued.take() {
                self.queued
                    .0
                    .insert(index.min(self.queued.0.len()), (receipt.id, handoff));
            }
            if let Some((index, handoff)) = receipt.previous_taken.take() {
                self.taken
                    .0
                    .insert(index.min(self.taken.0.len()), (receipt.id, handoff));
            }
            if let Some(token) = receipt.previous_token.take() {
                self.queue_tokens.set(receipt.id, token);
            }
        } else {
            receipt.previous_queued.take();
            receipt.previous_taken.take();
            receipt.previous_token.take();
        }
        receipt.token.status.set(QueueStatus::RolledBack);
        Ok(())
    }
    pub fn retire_superseded_handoff(&mut self, receipt: &mut GenerateQueueReceipt) {
        if self
            .queue_tokens
            .get(receipt.id)
            .is_some_and(|token| Rc::ptr_eq(token, &receipt.token))
        {
            self.queued.delete(receipt.id);
            self.taken.delete(receipt.id);
            self.queue_tokens.delete(receipt.id);
        }
        receipt.token.status.set(if receipt.was_consumed() {
            QueueStatus::RetiredConsumed
        } else {
            QueueStatus::RolledBack
        });
        receipt.previous_queued.take();
        receipt.previous_taken.take();
        receipt.previous_token.take();
    }

    /// Discard only this adopted insertion, without restoring displaced state.
    pub fn retire_adopted_handoff(
        &mut self,
        receipt: &mut GenerateQueueReceipt,
    ) -> Result<(), String> {
        let status = receipt.token.status.get();
        if matches!(
            status,
            QueueStatus::RolledBack | QueueStatus::RetiredConsumed
        ) {
            return Ok(());
        }
        if !self
            .queue_tokens
            .get(receipt.id)
            .is_some_and(|token| Rc::ptr_eq(token, &receipt.token))
        {
            if status == QueueStatus::Consumed {
                return Ok(());
            }
            return Err("The adopted generate handoff was replaced before close".into());
        }
        self.queued.delete(receipt.id);
        self.taken.delete(receipt.id);
        self.queue_tokens.delete(receipt.id);
        receipt
            .token
            .status
            .set(if status == QueueStatus::Consumed {
                QueueStatus::RetiredConsumed
            } else {
                QueueStatus::RolledBack
            });
        Ok(())
    }

    /// queue for a live webContents, and drop the request when it goes away first
    pub fn queue_generate_handoff_for(
        &mut self,
        contents: &mut dyn HandoffWebContents,
        handoff: GenerateHandoff,
    ) {
        if contents.is_destroyed() {
            return;
        }
        let id = contents.id();
        self.queue_generate_handoff(id, handoff);
        contents.once_destroyed();
    }

    /// The receiving editor's one take. Returns the request queued for this
    /// webContents when it was made for `target`, and empties the slot; `None`
    /// for anything else (nothing queued, already taken, another kind).
    pub fn take_generate_handoff(
        &mut self,
        target: GenerateHandoffTarget,
        web_contents_id: f64,
    ) -> Option<GenerateHandoff> {
        let handoff = self.queued.get(web_contents_id)?;
        if handoff.target != target {
            return None;
        }
        let handoff = self.queued.delete(web_contents_id)?;
        if let Some(token) = self.queue_tokens.get(web_contents_id) {
            token.status.set(QueueStatus::Consumed);
        }
        self.taken.set(web_contents_id, handoff.clone());
        Some(handoff)
    }

    /// the tab closed (or its webContents went away): its request, taken or not, is gone
    pub fn drop_generate_handoff(&mut self, web_contents_id: f64) {
        self.queued.delete(web_contents_id);
        self.taken.delete(web_contents_id);
        self.queue_tokens.delete(web_contents_id);
        self.skill_queued.delete(web_contents_id);
    }

    /// Numeric destruction notifications may clean only legacy entries. Owned
    /// insertions require their exact receipt or an exact native close proof.
    pub fn drop_legacy_handoff(&mut self, id: f64) {
        if !self.queue_tokens.get(id).is_some_and(|token| token.owned) {
            self.queued.delete(id);
            self.taken.delete(id);
            self.queue_tokens.delete(id);
        }
        self.skill_queued.delete(id);
    }

    /// queue a skill hand-off for a webContents; replaces one still queued there
    pub fn queue_skill_handoff(&mut self, web_contents_id: f64, handoff: SkillHandoff) {
        self.skill_queued.set(web_contents_id, handoff);
    }

    /// queue for a live webContents, and drop the request when it goes away first
    pub fn queue_skill_handoff_for(
        &mut self,
        contents: &mut dyn HandoffWebContents,
        handoff: SkillHandoff,
    ) {
        if contents.is_destroyed() {
            return;
        }
        let id = contents.id();
        self.queue_skill_handoff(id, handoff);
        contents.once_destroyed();
    }

    /// The receiving editor's one take of a skill hand-off: the request queued
    /// for this webContents when it was made for `target`, and the slot is
    /// emptied; `None` for anything else.
    pub fn take_skill_handoff(
        &mut self,
        target: HandoffEditor,
        web_contents_id: f64,
    ) -> Option<SkillHandoff> {
        let handoff = self.skill_queued.get(web_contents_id)?;
        if handoff.target.0 != target {
            return None;
        }
        self.skill_queued.delete(web_contents_id)
    }

    /// the skill hand-offs waiting to be taken, for diagnostics; no request text
    pub fn queued_skill_handoffs(&self) -> Vec<QueuedSkillHandoff> {
        self.skill_queued
            .0
            .iter()
            .map(|(id, h)| QueuedSkillHandoff {
                web_contents_id: *id,
                target: h.target,
                id: h.id.clone(),
            })
            .collect()
    }

    /// what is waiting to be taken, for diagnostics; no request text
    pub fn queued_generate_handoffs(&self) -> Vec<QueuedGenerateHandoff> {
        self.queued
            .0
            .iter()
            .map(|(id, h)| QueuedGenerateHandoff {
                web_contents_id: *id,
                target: h.target,
                id: h.id.clone(),
            })
            .collect()
    }

    /// the shell tells this module how to name a tab from its webContents id
    pub fn set_generate_tab_id_resolver(&mut self, resolver: Option<Box<TabIdResolver>>) {
        self.tab_id_of = resolver;
    }

    /// `tabIdOf?.(id)`; `Err` is a resolver that throws
    fn tab_id(&self, web_contents_id: f64) -> Result<Option<String>, String> {
        match &self.tab_id_of {
            Some(resolver) => resolver(web_contents_id),
            None => Ok(None),
        }
    }

    /// The receiving editor reports that a run started in this tab with the
    /// given attachments. Recorded only when the tab took a hand-off for
    /// `target` and at least one of the attached paths is a handed-over
    /// material; each hand-off is recorded once (its first run). Returns the
    /// record, or `None` when nothing was recorded; `Err` is the resolver's
    /// throw, after the hand-off was already marked as run.
    pub fn report_generate_run_started(
        &mut self,
        target: GenerateHandoffTarget,
        web_contents_id: f64,
        attached_paths: &[String],
        now_ms: f64,
    ) -> Result<Option<GenerateRunStarted>, String> {
        let Some(handoff) = self.taken.get(web_contents_id) else {
            return Ok(None);
        };
        if handoff.target != target {
            return Ok(None);
        }
        let attached: Vec<String> = attached_paths.iter().map(|p| self.path_key(p)).collect();
        let sources: Vec<String> = handoff
            .materials
            .iter()
            .map(|m| m.path.clone())
            .filter(|path| attached.contains(&self.path_key(path)))
            .collect();
        if sources.is_empty() {
            return Ok(None);
        }
        let handed_over: Vec<String> = handoff.materials.iter().map(|m| m.path.clone()).collect();
        let mode = handoff.mode.clone();
        let handoff_id = handoff.id.clone();
        self.taken.delete(web_contents_id);
        self.queue_tokens.delete(web_contents_id);
        let tab_id = self.tab_id(web_contents_id)?.filter(|id| !id.is_empty());
        let event = GenerateRunStarted {
            tab_id,
            web_contents_id,
            target,
            mode,
            sources,
            handed_over,
            handoff_id,
            at: now_ms,
        };
        self.runs.push(event.clone());
        if self.runs.len() > MAX_RUN_EVENTS {
            let excess = self.runs.len() - MAX_RUN_EVENTS;
            self.runs.drain(..excess);
        }
        Ok(Some(event))
    }

    /// Stage the immutable run record before entering the native owner gate.
    pub fn prepare_generate_run_started(
        &mut self,
        target: GenerateHandoffTarget,
        id: f64,
        attached_paths: &[String],
        now_ms: f64,
        tab_id: Result<Option<String>, String>,
    ) -> Result<Option<PreparedGenerateRun>, String> {
        let Some(handoff) = self.taken.get(id) else {
            return Ok(None);
        };
        if handoff.target != target {
            return Ok(None);
        }
        let attached: Vec<String> = attached_paths
            .iter()
            .map(|path| self.path_key(path))
            .collect();
        let sources: Vec<String> = handoff
            .materials
            .iter()
            .map(|material| material.path.clone())
            .filter(|path| attached.contains(&self.path_key(path)))
            .collect();
        if sources.is_empty() {
            return Ok(None);
        }
        let token = self
            .queue_tokens
            .get(id)
            .cloned()
            .ok_or("The taken handoff identity is unavailable")?;
        let result = tab_id.map(|tab_id| {
            let event = GenerateRunStarted {
                tab_id: tab_id.filter(|value| !value.is_empty()),
                web_contents_id: id,
                target,
                mode: handoff.mode.clone(),
                sources,
                handed_over: handoff
                    .materials
                    .iter()
                    .map(|material| material.path.clone())
                    .collect(),
                handoff_id: handoff.id.clone(),
                at: now_ms,
            };
            (event.clone(), event)
        });
        if result.is_ok() {
            self.runs
                .try_reserve(1)
                .map_err(|error| error.to_string())?;
        }
        Ok(Some(PreparedGenerateRun { id, token, result }))
    }

    pub fn commit_generate_run_started(
        &mut self,
        plan: PreparedGenerateRun,
    ) -> Result<Option<GenerateRunStarted>, String> {
        if !self
            .queue_tokens
            .get(plan.id)
            .is_some_and(|token| Rc::ptr_eq(token, &plan.token))
            || self.taken.get(plan.id).is_none()
        {
            return Err("The taken handoff changed before its run was recorded".into());
        }
        if plan.result.is_ok() && self.runs.len() == self.runs.capacity() {
            return Err("The run event capacity was consumed".into());
        }
        self.taken.delete(plan.id);
        self.queue_tokens.delete(plan.id);
        let (event, returned) = plan.result?;
        self.runs.push(event);
        if self.runs.len() > MAX_RUN_EVENTS {
            let excess = self.runs.len() - MAX_RUN_EVENTS;
            self.runs.drain(..excess);
        }
        Ok(Some(returned))
    }

    /// the recorded run-started events, oldest first (a copy)
    pub fn generate_run_started_events(&self) -> Vec<GenerateRunStarted> {
        self.runs.clone()
    }

    /// The shell opened a new tab with the result of a conversion without AI
    /// (8.6) queued for it. Recorded at once: the conversion itself is done by
    /// then. `Err` is a resolver that throws.
    pub fn record_generate_converted(
        &mut self,
        web_contents_id: f64,
        target: GenerateConvertTarget,
        kind: GenerateConvertKind,
        sources: &[String],
        conversion_id: &str,
        now_ms: f64,
    ) -> Result<GenerateConverted, String> {
        self.record_generate_converted_reversible(
            web_contents_id,
            target,
            kind,
            sources,
            conversion_id,
            now_ms,
        )
        .map(|(event, _)| event)
    }

    pub fn record_generate_converted_reversible(
        &mut self,
        web_contents_id: f64,
        target: GenerateConvertTarget,
        kind: GenerateConvertKind,
        sources: &[String],
        conversion_id: &str,
        now_ms: f64,
    ) -> Result<(GenerateConverted, ConvertedEventReceipt), String> {
        let event = self.prepare_generate_converted(
            web_contents_id,
            target,
            kind,
            sources,
            conversion_id,
            now_ms,
        )?;
        Ok(self.commit_generate_converted(event))
    }

    pub fn prepare_generate_converted(
        &mut self,
        web_contents_id: f64,
        target: GenerateConvertTarget,
        kind: GenerateConvertKind,
        sources: &[String],
        conversion_id: &str,
        now_ms: f64,
    ) -> Result<PreparedConvertedEvent, String> {
        let tab_id = self.tab_id(web_contents_id)?.filter(|id| !id.is_empty());
        let event = GenerateConverted {
            tab_id,
            web_contents_id,
            target,
            kind,
            mode: "convert".to_owned(),
            sources: sources.iter().filter(|p| !p.is_empty()).cloned().collect(),
            conversion_id: conversion_id.to_owned(),
            at: now_ms,
        };
        self.converted
            .try_reserve(1)
            .map_err(|error| error.to_string())?;
        let mut evicted = Vec::new();
        evicted.try_reserve(1).map_err(|error| error.to_string())?;
        Ok(PreparedConvertedEvent {
            returned: event.clone(),
            event,
            token: Rc::new(()),
            evicted,
        })
    }

    pub fn can_commit_generate_converted(&self) -> bool {
        self.converted.len() < self.converted.capacity() && self.converted.len() <= MAX_RUN_EVENTS
    }

    pub fn commit_generate_converted(
        &mut self,
        plan: PreparedConvertedEvent,
    ) -> (GenerateConverted, ConvertedEventReceipt) {
        let PreparedConvertedEvent {
            event,
            returned,
            token,
            mut evicted,
        } = plan;
        self.converted.push((token.clone(), event));
        let excess = self.converted.len().saturating_sub(MAX_RUN_EVENTS);
        evicted.extend(self.converted.drain(..excess));
        (
            returned,
            ConvertedEventReceipt {
                token,
                evicted,
                done: false,
            },
        )
    }

    pub fn rollback_generate_converted(
        &mut self,
        receipt: &mut ConvertedEventReceipt,
    ) -> Result<(), String> {
        if receipt.done {
            return Ok(());
        }
        let index = self
            .converted
            .iter()
            .position(|(token, _)| Rc::ptr_eq(token, &receipt.token))
            .ok_or("The converted event was replaced or evicted before rollback")?;
        if self.converted.len() - 1 + receipt.evicted.len() > MAX_RUN_EVENTS {
            return Err("The converted event cannot restore its displaced history".into());
        }
        self.converted.remove(index);
        self.converted.splice(0..0, receipt.evicted.drain(..));
        receipt.done = true;
        Ok(())
    }

    /// the recorded conversions, oldest first (a copy)
    pub fn generate_converted_events(&self) -> Vec<GenerateConverted> {
        self.converted
            .iter()
            .map(|(_, event)| event.clone())
            .collect()
    }

    /// tests only: forget everything
    pub fn reset_generate_handoffs_for_test(&mut self) {
        self.queued = IdMap::new();
        self.taken = IdMap::new();
        self.queue_tokens = IdMap::new();
        self.skill_queued = IdMap::new();
        self.runs.clear();
        self.converted.clear();
        self.tab_id_of = None;
    }
}

#[cfg(test)]
mod rollback_tests {
    use super::*;
    fn handoff(id: &str) -> GenerateHandoff {
        GenerateHandoff {
            id: id.into(),
            mode: "make".into(),
            target: GenerateHandoffTarget::Mindmap,
            materials: Vec::new(),
            text: String::new(),
            default_text: false,
            created_at: 0.0,
        }
    }
    #[test]
    fn staged_run_rejects_replacement_and_preserves_resolver_error_semantics() {
        let mut store = HandoffStore::new(false);
        let mut first = handoff("first");
        first.materials.push(Material {
            path: "/source.md".into(),
            name: "source.md".into(),
        });
        store.queue_generate_handoff(2.0, first.clone());
        store
            .take_generate_handoff(GenerateHandoffTarget::Mindmap, 2.0)
            .unwrap();
        let plan = store
            .prepare_generate_run_started(
                GenerateHandoffTarget::Mindmap,
                2.0,
                &["/source.md".into()],
                0.0,
                Ok(Some("tab".into())),
            )
            .unwrap()
            .unwrap();
        store.queue_generate_handoff(2.0, first.clone());
        store
            .take_generate_handoff(GenerateHandoffTarget::Mindmap, 2.0)
            .unwrap();
        assert!(store.commit_generate_run_started(plan).is_err());
        assert!(store.generate_run_started_events().is_empty());
        let failed = store
            .prepare_generate_run_started(
                GenerateHandoffTarget::Mindmap,
                2.0,
                &["/source.md".into()],
                0.0,
                Err("resolver failed".into()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            store.commit_generate_run_started(failed).unwrap_err(),
            "resolver failed"
        );
        assert!(store.taken.get(2.0).is_none());
        store.queue_generate_handoff(2.0, first);
        store
            .take_generate_handoff(GenerateHandoffTarget::Mindmap, 2.0)
            .unwrap();
        let plan = store
            .prepare_generate_run_started(
                GenerateHandoffTarget::Mindmap,
                2.0,
                &["/source.md".into()],
                1.0,
                Ok(Some("tab".into())),
            )
            .unwrap()
            .unwrap();
        let event = store.commit_generate_run_started(plan).unwrap().unwrap();
        assert_eq!(event.tab_id.as_deref(), Some("tab"));
        assert_eq!(event.sources, vec!["/source.md"]);
        assert!(
            store
                .prepare_generate_run_started(
                    GenerateHandoffTarget::Mindmap,
                    2.0,
                    &["/source.md".into()],
                    2.0,
                    Ok(None)
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restores_taken_entry_and_refuses_equal_replacement() {
        let mut store = HandoffStore::new(false);
        store.queue_generate_handoff(2.0, handoff("old"));
        store
            .take_generate_handoff(GenerateHandoffTarget::Mindmap, 2.0)
            .unwrap();
        let mut receipt = store.queue_generate_handoff_reversible(2.0, handoff("same"));
        store.queue_generate_handoff(2.0, handoff("same"));
        assert!(store.rollback_generate_handoff(&mut receipt).is_err());
        assert_eq!(store.queued.get(2.0).unwrap().id, "same");
        let mut replacement = store.queue_generate_handoff_reversible(2.0, handoff("new"));
        store.rollback_generate_handoff(&mut replacement).unwrap();
        assert_eq!(store.queued.get(2.0).unwrap().id, "same");
    }
    #[test]
    fn restores_taken_history_and_preserves_consumed_transfer() {
        let mut store = HandoffStore::new(false);
        store.queue_generate_handoff(2.0, handoff("old"));
        store
            .take_generate_handoff(GenerateHandoffTarget::Mindmap, 2.0)
            .unwrap();
        let mut receipt = store.queue_generate_handoff_reversible(2.0, handoff("new"));
        store.rollback_generate_handoff(&mut receipt).unwrap();
        assert_eq!(store.taken.get(2.0).unwrap().id, "old");
        let mut consumed = store.queue_generate_handoff_reversible(2.0, handoff("accepted"));
        store
            .take_generate_handoff(GenerateHandoffTarget::Mindmap, 2.0)
            .unwrap();
        store.rollback_generate_handoff(&mut consumed).unwrap();
        assert_eq!(store.taken.get(2.0).unwrap().id, "accepted");
    }
    #[test]
    fn superseded_displaced_queue_is_not_restored_by_successor_rollback() {
        let mut store = HandoffStore::new(false);
        let mut old = store.queue_generate_handoff_reversible(2.0, handoff("old"));
        let mut next = store.queue_generate_handoff_reversible(2.0, handoff("next"));
        store.retire_superseded_handoff(&mut old);
        assert_eq!(store.queued.get(2.0).unwrap().id, "next");
        store.rollback_generate_handoff(&mut next).unwrap();
        assert!(store.queued.get(2.0).is_none());
        assert!(store.taken.get(2.0).is_none());
        let mut consumed = store.queue_generate_handoff_reversible(2.0, handoff("consumed"));
        store
            .take_generate_handoff(GenerateHandoffTarget::Mindmap, 2.0)
            .unwrap();
        let mut later = store.queue_generate_handoff_reversible(2.0, handoff("later"));
        store.retire_superseded_handoff(&mut consumed);
        assert!(consumed.was_consumed());
        store.rollback_generate_handoff(&mut later).unwrap();
        assert!(store.taken.get(2.0).is_none());
    }

    #[test]
    fn conversion_receipt_removes_only_its_exact_event_and_restores_evicted() {
        let mut store = HandoffStore::new(false);
        for i in 0..MAX_RUN_EVENTS {
            store
                .record_generate_converted(
                    1.0,
                    GenerateConvertTarget::Markdown,
                    GenerateConvertKind::MindmapToMarkdown,
                    &[],
                    &i.to_string(),
                    0.0,
                )
                .unwrap();
        }
        let (_, mut receipt) = store
            .record_generate_converted_reversible(
                1.0,
                GenerateConvertTarget::Markdown,
                GenerateConvertKind::MindmapToMarkdown,
                &[],
                "same",
                0.0,
            )
            .unwrap();
        store.rollback_generate_converted(&mut receipt).unwrap();
        store.rollback_generate_converted(&mut receipt).unwrap();
        let events = store.generate_converted_events();
        assert_eq!(events.len(), MAX_RUN_EVENTS);
        assert_eq!(events[0].conversion_id, "0");
        assert!(events.iter().all(|event| event.conversion_id != "same"));
    }
    #[test]
    fn numeric_teardown_removes_legacy_but_preserves_owned_receipt() {
        let mut store = HandoffStore::new(false);
        store.queue_generate_handoff(1.0, handoff("legacy"));
        let mut owned = store.queue_generate_handoff_reversible(2.0, handoff("owned"));
        store.drop_legacy_handoff(1.0);
        store.drop_legacy_handoff(2.0);
        assert!(store.queued.get(1.0).is_none());
        assert_eq!(store.queued.get(2.0).unwrap().id, "owned");
        store.rollback_generate_handoff(&mut owned).unwrap();
        assert!(store.queued.get(2.0).is_none());
    }
}
