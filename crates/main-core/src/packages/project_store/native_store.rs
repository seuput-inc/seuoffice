use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{canonical_path_key, is_valid_project_id, nfc};

#[path = "chat_files.rs"]
mod chat_files;
#[path = "native_store/owned_rename.rs"]
pub mod owned_rename;
use owned_rename::RenameIo as _;

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("system time before epoch").as_millis() as f64
}

fn now_iso() -> String {
    jscompat::to_iso_string(now_ms()).expect("system time outside JavaScript date range")
}

fn safe_id<'a>(id: &'a str, kind: &str) -> Result<&'a str, String> {
    if is_valid_project_id(id) { Ok(id) } else {
        Err(format!("Invalid {kind} \"{id}\": must be non-empty and match /^[A-Za-z0-9_-]+$/ (rejects \"..\", \"/\" and backslash)"))
    }
}

fn hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let mut admit = |effect: &mut dyn FnMut()| { effect(); Ok(()) };
    owned_rename::NativeIo { admit:&mut admit }.json(path,value)
}

fn mapped_key(map: Option<&Value>, path: &str) -> Option<String> {
    let map = map?.as_object()?;
    let canonical = canonical_path_key(path);
    [canonical, path.to_owned(), nfc(path)].into_iter().find(|key| map.contains_key(key))
}

fn clamp(value: Option<&Value>, max: usize) -> Value {
    Value::String(jscompat::slice_utf16(value.and_then(Value::as_str).unwrap_or(""), 0.0, Some(max as f64)))
}

fn sanitize_message(record: &mut Value, message: &Value) {
    if let Some(file) = message.get("fileRef") { record["fileRef"] = file.clone(); }
    if let Some(tools) = message.get("tools").and_then(Value::as_array).filter(|tools| !tools.is_empty()) {
        record["tools"] = Value::Array(tools.iter().take(64).map(|tool| {
            let mut out = tool.as_object().cloned().unwrap_or_default();
            out.insert("name".into(), clamp(tool.get("name"), 200));
            out.insert("summary".into(), clamp(tool.get("summary"), 2000));
            for field in ["input", "output"] {
                if let Some(value) = tool.get(field) {
                    let text = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
                    out.insert(field.into(), clamp(Some(&Value::String(text)), 16000));
                }
            }
            Value::Object(out)
        }).collect());
    }
    if let Some(attachments) = message.get("attachments").and_then(Value::as_array) {
        record["attachments"] = Value::Array(attachments.iter().take(32).map(|attachment| {
            let mut out = attachment.as_object().cloned().unwrap_or_default();
            out.insert("name".into(), clamp(attachment.get("name"), 1000));
            for field in ["path", "ext"] {
                if let Some(value) = attachment.get(field) { out.insert(field.into(), clamp(Some(value), 1000)); }
            }
            Value::Object(out)
        }).collect());
    }
    if let Some(scope) = message.get("scope") {
        let mut out = serde_json::Map::new();
        out.insert("label".into(), clamp(scope.get("label"), 200));
        if let Some(text) = scope.get("text") { out.insert("text".into(), clamp(Some(text), 400)); }
        record["scope"] = Value::Object(out);
    }
    if let Some(skills) = message.get("skills").and_then(Value::as_array) {
        let chips: Vec<Value> = skills.iter().filter_map(|skill| {
            let id = skill.get("id")?.as_str()?;
            if id.is_empty() || id.encode_utf16().count() > 512 { return None; }
            Some(json!({"id":id,"source":clamp(skill.get("source"), 200),"title":clamp(skill.get("title"), 200)}))
        }).take(32).collect();
        if !chips.is_empty() { record["skills"] = Value::Array(chips); }
    }
}

pub struct NativeProjectStore {
    base: PathBuf,
    sequences: HashMap<(String, String), f64>,
    pending: HashMap<(String, String), Vec<Value>>,
    revision: Arc<AtomicU64>,
}

impl NativeProjectStore {
    pub fn new(user_data: impl AsRef<Path>) -> Self {
        Self { base: user_data.as_ref().join("projects"), sequences: HashMap::new(), pending: HashMap::new(), revision:Arc::new(AtomicU64::new(0)) }
    }

    fn project_dir(&self, id: &str) -> Result<PathBuf, String> {
        Ok(self.base.join(safe_id(id, "projectId")?))
    }

    fn chat_path(&self, project: &str, chat: &str) -> Result<PathBuf, String> {
        Ok(self.project_dir(project)?.join("chats").join(format!("{}.jsonl", safe_id(chat, "chatId")?)))
    }

    fn index(&self) -> Value {
        read_json(&self.base.join("index.json")).unwrap_or_else(|| json!({"projects": [], "fileMap": {}}))
    }

    fn write_index(&self, index: &Value) -> Result<(), String> {
        write_json(&self.base.join("index.json"), index)
    }

    fn project(&self, id: &str) -> Result<Option<Value>, String> {
        Ok(read_json(&self.project_dir(id)?.join("project.json")))
    }

    fn write_project(&self, project: &Value) -> Result<(), String> {
        let mut admit = |effect: &mut dyn FnMut()| { effect(); Ok(()) };
        self.write_project_with_io(project,&mut owned_rename::NativeIo { admit:&mut admit })
    }

    fn write_project_with_io(&self,project:&Value,io:&mut dyn owned_rename::RenameIo)->Result<(),String> {
        let id = project.get("id").and_then(Value::as_str).ok_or("project missing id")?;
        io.json(&self.project_dir(id)?.join("project.json"),project)
    }

    pub fn ensure_default_project(&self) -> Result<Value, String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        if let Some(project) = self.project("default")? { return Ok(project); }
        let timestamp = now_iso();
        let project = json!({"id":"default", "name":"Default Project", "createdAt":timestamp, "updatedAt":timestamp, "files":[]});
        let path = self.project_dir("default")?.join("project.json");
        if path.exists() {
            let mut aside = path.as_os_str().to_os_string();
            aside.push(format!(".corrupt-{}", now_ms() as u64));
            fs::rename(&path, PathBuf::from(aside)).map_err(|e| e.to_string())?;
        }
        self.write_project(&project)?;
        let mut index = self.index();
        let projects = index.get_mut("projects").and_then(Value::as_array_mut).ok_or("invalid projects index")?;
        if !projects.iter().any(|entry| entry.get("id").and_then(Value::as_str) == Some("default")) {
            projects.insert(0, json!({"id":"default", "name":"Default Project", "createdAt":timestamp, "updatedAt":timestamp}));
            self.write_index(&index)?;
        }
        Ok(project)
    }

    fn fallback_chat_id(&self, project: &str, path: &str) -> Result<String, String> {
        let canonical = hash(&canonical_path_key(path));
        if self.chat_path(project, &canonical)?.exists() { return Ok(canonical); }
        for id in [hash(path), hash(&nfc(path))] {
            if id != canonical && self.chat_path(project, &id)?.exists() { return Ok(id); }
        }
        Ok(canonical)
    }

    pub fn lookup_chat_for_file(&self, path: &str) -> Result<Value, String> {
        let index = self.index();
        let project = mapped_key(index.get("fileMap"), path)
            .and_then(|key| index["fileMap"][key].as_str().map(str::to_owned))
            .unwrap_or_else(|| "default".to_owned());
        let chat = match mapped_key(index.get("chatIdByPath"), path) {
            Some(key) => index["chatIdByPath"][key].as_str().ok_or("invalid chat id")?.to_owned(),
            None => self.fallback_chat_id(&project, path)?,
        };
        Ok(json!({"projectId":project,"chatId":chat}))
    }

    pub fn resolve_chat_for_file(&self, path: &str) -> Result<Value, String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        self.ensure_default_project()?;
        let mut index = self.index();
        let canonical = canonical_path_key(path);
        let old = mapped_key(index.get("fileMap"), path);
        let project = old.as_ref().and_then(|key| index["fileMap"][key].as_str()).unwrap_or("default").to_owned();
        if old.as_deref() != Some(canonical.as_str()) {
            let map = index.get_mut("fileMap").and_then(Value::as_object_mut).ok_or("invalid file map")?;
            if let Some(key) = old.as_ref() { map.remove(key); }
            map.insert(canonical.clone(), Value::String(project.clone()));
            self.write_index(&index)?;
        }
        if old.is_none() {
            if let Some(mut data) = self.project("default")? {
                let files = data.get_mut("files").and_then(Value::as_array_mut).ok_or("invalid project files")?;
                if !files.iter().filter_map(Value::as_str).any(|file| canonical_path_key(file) == canonical) {
                    files.push(Value::String(path.to_owned()));
                    data["updatedAt"] = Value::String(now_iso());
                    self.write_project(&data)?;
                }
            }
        }
        let old_chat = mapped_key(index.get("chatIdByPath"), path);
        let chat = match &old_chat {
            Some(key) => index["chatIdByPath"][key].as_str().ok_or("invalid chat id")?.to_owned(),
            None => self.fallback_chat_id(&project, path)?,
        };
        if old_chat.as_deref() != Some(canonical.as_str()) {
            if !index.get("chatIdByPath").is_some_and(Value::is_object) { index["chatIdByPath"] = json!({}); }
            let chats = index["chatIdByPath"].as_object_mut().ok_or("invalid chat map")?;
            if let Some(key) = old_chat { chats.remove(&key); }
            chats.insert(canonical, Value::String(chat.clone()));
            self.write_index(&index)?;
        }
        Ok(json!({"projectId":project,"chatId":chat}))
    }

    pub fn load_chat(&self, project: &str, chat: &str, limit: f64) -> Result<Vec<Value>, String> {
        let path = self.chat_path(project, chat)?;
        let key = (project.to_owned(), chat.to_owned());
        let mut messages = self.pending.get(&key).cloned().unwrap_or_default();
        if let Ok(disk) = chat_files::read_tail(&path) {
            messages.extend(disk);
            messages.sort_by(|a, b| {
                a["seq"].as_f64().unwrap_or(-1.0).total_cmp(&b["seq"].as_f64().unwrap_or(-1.0))
            });
            let limit = if limit.is_finite() { limit.floor().clamp(1.0, 10000.0) as usize } else { 200 };
            if messages.len() > limit { messages.drain(..messages.len() - limit); }
        }
        Ok(messages)
    }

    pub fn append_chat_message(&mut self, project: &str, chat: &str, message: &Value) -> Result<(), String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        let path = self.chat_path(project, chat)?;
        let key = (project.to_owned(), chat.to_owned());
        let seq = match self.sequences.get(&key) {
            Some(seq) => *seq + 1.0,
            None => self.load_chat(project, chat, 10000.0)?.iter()
                .filter_map(|item| item["seq"].as_f64()).fold(-1.0_f64, f64::max) + 1.0,
        };
        self.sequences.insert(key.clone(), seq);
        let role = message.get("role").and_then(Value::as_str).ok_or("message role must be a string")?;
        let text = message.get("text").and_then(Value::as_str).ok_or("message text must be a string")?;
        let text = if text.encode_utf16().count() > 32000 {
            format!("{}\n\n[truncated]", jscompat::slice_utf16(text, 0.0, Some(32000.0)))
        } else { text.to_owned() };
        let mut record = json!({"seq":seq,"ts":message.get("ts").cloned().unwrap_or_else(|| Value::String(now_iso())),"role":role,"text":text});
        sanitize_message(&mut record, message);
        let mut records = self.pending.get(&key).cloned().unwrap_or_default();
        records.push(record);
        if role != "assistant" && !path.exists() {
            if records.len() < 200 {
                self.pending.insert(key, records);
                return Ok(());
            }
            self.pending.insert(key.clone(), records.clone());
        }
        chat_files::append(&path, &records).map_err(|error| error.to_string())?;
        self.pending.remove(&key);
        Ok(())
    }

    fn flush_pending(&mut self, project: &str, chat: &str) -> Result<(), String> {
        let path = self.chat_path(project, chat)?;
        let key = (project.to_owned(), chat.to_owned());
        if let Some(records) = self.pending.remove(&key) {
            if let Err(error) = chat_files::append(&path, &records) {
                eprintln!("[project-store] flushPending failed: {error}");
            }
        }
        Ok(())
    }

    pub fn rebind_chat(&mut self, from_project: &str, from_chat: &str, to_project: &str, to_chat: &str) -> Result<(), String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        if from_project == to_project && from_chat == to_chat { return Ok(()); }
        let source = self.chat_path(from_project, from_chat)?;
        let target = self.chat_path(to_project, to_chat)?;
        self.flush_pending(from_project, from_chat)?;
        self.flush_pending(to_project, to_chat)?;
        let mut merged = None;
        if source.exists() {
            let result = (|| -> io::Result<()> {
                fs::create_dir_all(target.parent().ok_or_else(|| io::Error::other("chat has no parent"))?)?;
                if target.exists() { merged = chat_files::merge(&source, &target)?; }
                else { fs::rename(&source, &target)?; }
                Ok(())
            })();
            if let Err(error) = result { eprintln!("[project-store] rebindChat rename failed: {error}"); }
        }
        let old = (from_project.to_owned(), from_chat.to_owned());
        let new = (to_project.to_owned(), to_chat.to_owned());
        let current = self.sequences.remove(&old);
        self.sequences.remove(&new);
        if let Some(seq) = merged.or(current) { self.sequences.insert(new, seq); }
        Ok(())
    }

    pub fn rebind_chat_to_file(&mut self, project: &str, chat: &str, path: &str) -> Result<Value, String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        let result = self.resolve_chat_for_file(path)?;
        let target_project = result["projectId"].as_str().ok_or("missing project id")?;
        let target_chat = result["chatId"].as_str().ok_or("missing chat id")?;
        self.rebind_chat(project, chat, target_project, target_chat)?;
        Ok(result)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Value>, String> {
        self.project(id)
    }

    pub fn list_projects(&self) -> Vec<Value> {
        self.index().get("projects").and_then(Value::as_array).cloned().unwrap_or_default()
    }

    pub fn list_project_files(&self, id: &str) -> Result<Vec<String>, String> {
        let Some(project) = self.project(id)? else { return Ok(Vec::new()); };
        let mut files = Vec::new();
        if let Some(entries) = project.get("files").and_then(Value::as_array) {
            for path in entries.iter().filter_map(Value::as_str) {
                if Path::new(path).exists() && !files.iter().any(|file| file == path) {
                    files.push(path.to_owned());
                }
            }
        }
        Ok(files)
    }

    pub fn create_project(&self, name: &str) -> Result<Value, String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        let name = name.trim();
        if name.is_empty() { return Err("Project name cannot be empty".into()); }
        let length = name.encode_utf16().count();
        if length > 128 { return Err(format!("Project name too long: {length} chars (max 128)")); }
        let timestamp = now_iso();
        let mut index = self.index();
        let projects = index.get_mut("projects").and_then(Value::as_array_mut).ok_or("invalid projects index")?;
        let mut attempt = 0;
        let id = loop {
            let source = if attempt == 0 { format!("{name}{timestamp}") } else { format!("{name}:{timestamp}:{attempt}") };
            let id = format!("proj-{}", &hash(&source)[..12]);
            if !projects.iter().any(|entry| entry["id"].as_str() == Some(id.as_str())) { break id; }
            attempt += 1;
        };
        let project = json!({"id":id,"name":name,"createdAt":timestamp,"updatedAt":timestamp,"files":[]});
        self.write_project(&project)?;
        projects.push(json!({"id":id,"name":name,"createdAt":timestamp,"updatedAt":timestamp}));
        self.write_index(&index)?;
        Ok(project)
    }

    pub fn rename_project(&self, id: &str, name: &str) -> Result<(), String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        if id == "default" { return Err("The default project cannot be renamed".into()); }
        let name = name.trim();
        if name.is_empty() { return Err("Project name cannot be empty".into()); }
        let length = name.encode_utf16().count();
        if length > 128 { return Err(format!("Project name too long: {length} chars (max 128)")); }
        let mut project = self.project(id)?.ok_or_else(|| format!("Project does not exist: {id}"))?;
        let now = now_iso();
        project["name"] = Value::String(name.to_owned());
        project["updatedAt"] = Value::String(now.clone());
        self.write_project(&project)?;
        let mut index = self.index();
        if let Some(projects) = index.get_mut("projects").and_then(Value::as_array_mut) {
            if let Some(entry) = projects.iter_mut().find(|entry| entry["id"].as_str() == Some(id)) {
                entry["name"] = Value::String(name.to_owned());
                entry["updatedAt"] = Value::String(now);
            }
        }
        self.write_index(&index)
    }

    pub fn move_file_to_project(&mut self, path: &str, target: &str) -> Result<(), String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        self.ensure_default_project()?;
        let mut index = self.index();
        let old_key = mapped_key(index.get("fileMap"), path);
        let source = old_key.as_ref().and_then(|key| index["fileMap"][key].as_str())
            .unwrap_or("default").to_owned();
        if source == target { return Ok(()); }
        let mut target_data = self.project(target)?
            .ok_or_else(|| format!("Target project does not exist: {target}"))?;
        let canonical = canonical_path_key(path);
        let file_map = index.get_mut("fileMap").and_then(Value::as_object_mut).ok_or("invalid file map")?;
        if let Some(key) = old_key { file_map.remove(&key); }
        file_map.insert(canonical.clone(), Value::String(target.to_owned()));
        self.write_index(&index)?;
        if let Some(mut source_data) = self.project(&source)? {
            let files = source_data.get_mut("files").and_then(Value::as_array_mut).ok_or("invalid project files")?;
            files.retain(|file| file.as_str().is_none_or(|file| canonical_path_key(file) != canonical));
            source_data["updatedAt"] = Value::String(now_iso());
            self.write_project(&source_data)?;
        }
        let files = target_data.get_mut("files").and_then(Value::as_array_mut).ok_or("invalid project files")?;
        if !files.iter().filter_map(Value::as_str).any(|file| canonical_path_key(file) == canonical) {
            files.push(Value::String(path.to_owned()));
        }
        target_data["updatedAt"] = Value::String(now_iso());
        self.write_project(&target_data)?;
        let chat = match mapped_key(index.get("chatIdByPath"), path) {
            Some(key) => index["chatIdByPath"][key].as_str().ok_or("invalid chat id")?.to_owned(),
            None => self.fallback_chat_id(&source, path)?,
        };
        self.rebind_chat(&source, &chat, target, &chat)
    }

    pub fn delete_project(&mut self, id: &str) -> Result<(), String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        if id == "default" { return Err("The default project cannot be deleted".into()); }
        let project = self.project(id)?.ok_or_else(|| format!("Project does not exist: {id}"))?;
        self.ensure_default_project()?;
        let mut index = self.index();
        let files: Vec<String> = index.get("fileMap").and_then(Value::as_object)
            .ok_or("invalid file map")?.iter()
            .filter(|(_, owner)| owner.as_str() == Some(id))
            .map(|(path, _)| path.clone()).collect();
        for path in &files {
            let chat = match mapped_key(index.get("chatIdByPath"), path) {
                Some(key) => index["chatIdByPath"][key].as_str().ok_or("invalid chat id")?.to_owned(),
                None => self.fallback_chat_id(id, path)?,
            };
            self.rebind_chat(id, &chat, "default", &chat)?;
        }
        let source = self.project_dir(id)?;
        let trash = self.base.join(".trash");
        fs::create_dir_all(&trash).map_err(|error| error.to_string())?;
        if source.exists() {
            if let Err(error) = fs::rename(source, trash.join(format!("{id}-{}", now_ms() as u64))) {
                eprintln!("[project-store] deleteProject rename to trash failed: {error}");
            }
        }
        let map = index.get_mut("fileMap").and_then(Value::as_object_mut).ok_or("invalid file map")?;
        for path in &files { map.insert(path.clone(), Value::String("default".to_owned())); }
        if !files.is_empty() {
            if let Some(mut default) = self.project("default")? {
                let entries = default.get_mut("files").and_then(Value::as_array_mut).ok_or("invalid project files")?;
                for path in &files {
                    let display = project.get("files").and_then(Value::as_array).and_then(|files| {
                        files.iter().filter_map(Value::as_str).find(|listed| canonical_path_key(listed) == *path)
                    }).unwrap_or(path);
                    if !entries.iter().filter_map(Value::as_str).any(|file| canonical_path_key(file) == *path) {
                        entries.push(Value::String(display.to_owned()));
                    }
                }
                default["updatedAt"] = Value::String(now_iso());
                self.write_project(&default)?;
            }
        }
        let projects = index.get_mut("projects").and_then(Value::as_array_mut).ok_or("invalid projects index")?;
        projects.retain(|entry| entry["id"].as_str() != Some(id));
        self.write_index(&index)
    }

    pub fn chat_id_for_path(&self, path: &str, project: Option<&str>) -> Result<String, String> {
        let index = self.index();
        match mapped_key(index.get("chatIdByPath"), path) {
            Some(key) => index["chatIdByPath"][key].as_str().map(str::to_owned).ok_or_else(|| "invalid chat id".to_owned()),
            None => match project {
                Some(project) => self.fallback_chat_id(project, path),
                None => Ok(hash(&canonical_path_key(path))),
            },
        }
    }

    pub fn list_projects_summary(&self) -> Result<Vec<Value>, String> {
        self.ensure_default_project()?;
        let mut summaries = Vec::new();
        for mut info in self.list_projects() {
            let id = info.get("id").and_then(Value::as_str).ok_or("project missing id")?.to_owned();
            let count = self.list_project_files(&id)?.len();
            let mut active = info.get("updatedAt").and_then(Value::as_str).unwrap_or("").to_owned();
            for chat in self.list_chats(&id)? {
                if let Some(updated) = chat.get("updatedAt").and_then(Value::as_str) {
                    if updated > active.as_str() { active = updated.to_owned(); }
                }
            }
            info["fileCount"] = json!(count);
            info["lastActiveAt"] = Value::String(active);
            info["isDefault"] = Value::Bool(id == "default");
            summaries.push(info);
        }
        Ok(summaries)
    }

    pub fn project_timeline(&self, project: &str, limit: f64) -> Result<Vec<Value>, String> {
        let limit = if limit.is_finite() { limit.floor().clamp(1.0, 1000.0) as usize } else { 20 };
        let index = self.index();
        let mut paths = HashMap::new();
        if let Some(map) = index.get("fileMap").and_then(Value::as_object) {
            for (path, owner) in map {
                if owner.as_str() == Some(project) {
                    paths.insert(self.chat_id_for_path(path, Some(project))?, path.clone());
                }
            }
        }
        let mut entries = Vec::new();
        for chat in self.list_chats(project)? {
            let id = chat.get("chatId").and_then(Value::as_str).ok_or("chat missing id")?;
            let path = paths.get(id).map(String::as_str).unwrap_or("");
            let name = if path.is_empty() { id.to_owned() } else { jscompat::node_path::host::basename(path, None) };
            for message in self.load_chat(project, id, limit as f64)? {
                let preview = jscompat::slice_utf16(message["text"].as_str().unwrap_or(""), 0.0, Some(120.0));
                entries.push(json!({"filePath":path,"fileName":name,"chatId":id,
                    "ts":message["ts"],"role":message["role"],"preview":preview,"seq":message["seq"]}));
            }
        }
        entries.sort_by(|a, b| {
            b["ts"].as_str().unwrap_or("").cmp(a["ts"].as_str().unwrap_or(""))
                .then_with(|| b["seq"].as_f64().unwrap_or(0.0).total_cmp(&a["seq"].as_f64().unwrap_or(0.0)))
        });
        entries.truncate(limit);
        Ok(entries)
    }

    pub fn known_file_paths(&self) -> Vec<String> {
        let index = self.index();
        let mut result = Vec::new();
        for map in [index.get("fileMap"), index.get("chatIdByPath")] {
            if let Some(map) = map.and_then(Value::as_object) {
                for key in map.keys() {
                    if !result.contains(key) { result.push(key.clone()); }
                }
            }
        }
        result
    }

    pub fn list_chats(&self, project: &str) -> Result<Vec<Value>, String> {
        let dir = self.project_dir(project)?.join("chats");
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(Vec::new()),
        };
        let mut chats = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(chat_id) = name.strip_suffix(".jsonl") else { continue; };
            let (updated, count) = match entry.metadata() {
                Ok(meta) => {
                    let updated = meta.modified().ok()
                        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                        .and_then(|at| jscompat::to_iso_string(at.as_millis() as f64))
                        .unwrap_or_else(now_iso);
                    (updated, (meta.len() as f64 / 120.0).round())
                }
                Err(_) => (now_iso(), 0.0),
            };
            chats.push(json!({"chatId":chat_id,"updatedAt":updated,"approxCount":count}));
        }
        Ok(chats)
    }

    pub fn legacy_chat_for(&self, path: &str) -> Result<Option<Value>, String> {
        let index = self.index();
        let mut projects = vec!["default".to_owned()];
        if let Some(entries) = index.get("projects").and_then(Value::as_array) {
            for project in entries {
                if let Some(id) = project.get("id").and_then(Value::as_str) {
                    if !projects.iter().any(|existing| existing == id) { projects.push(id.to_owned()); }
                }
            }
        }
        for project in projects {
            let chat = match mapped_key(index.get("chatIdByPath"), path) {
                Some(key) => index["chatIdByPath"][key].as_str().ok_or("invalid chat id")?.to_owned(),
                None => self.fallback_chat_id(&project, path)?,
            };
            if let Some(mut item) = self.list_chats(&project)?.into_iter()
                .find(|item| item["chatId"].as_str() == Some(chat.as_str()))
            {
                item["projectId"] = Value::String(project);
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    pub fn file_renamed(&mut self, old_path: &str, new_path: &str) -> Result<(), String> {
        self.revision.fetch_add(1,Ordering::AcqRel);
        let mut admit = |effect: &mut dyn FnMut()| { effect(); Ok(()) };
        self.file_renamed_with_io(old_path,new_path,&mut owned_rename::NativeIo { admit:&mut admit })
    }

    fn file_renamed_with_io(&mut self,old_path:&str,new_path:&str,io:&mut dyn owned_rename::RenameIo)->Result<(),String> {
        if old_path == new_path { return Ok(()); }
        let mut index = self.index();
        let old = canonical_path_key(old_path);
        let new = canonical_path_key(new_path);
        let source_key = mapped_key(index.get("fileMap"), old_path);
        let dest_key = mapped_key(index.get("fileMap"), new_path);
        let source_project = source_key.as_ref().and_then(|key| index["fileMap"][key].as_str()).map(str::to_owned);
        let dest_project = dest_key.as_ref().filter(|key| Some(*key) != source_key.as_ref())
            .and_then(|key| index["fileMap"][key].as_str()).map(str::to_owned);
        let source_chat_key = mapped_key(index.get("chatIdByPath"), old_path);
        let dest_chat_key = mapped_key(index.get("chatIdByPath"), new_path);
        let displaced_chat = dest_chat_key.as_ref().filter(|key| Some(*key) != source_chat_key.as_ref())
            .and_then(|key| index["chatIdByPath"][key].as_str()).map(str::to_owned);
        if let Some(project) = source_project.as_deref() {
            let files = index.get_mut("fileMap").and_then(Value::as_object_mut).ok_or("invalid file map")?;
            if let Some(key) = source_key { files.remove(&key); }
            if dest_project.is_some() { if let Some(key) = dest_key { files.remove(&key); } }
            files.insert(new.clone(), Value::String(project.to_owned()));
            if let Some(displaced) = dest_project.as_deref().filter(|id| *id != project) {
                if let Some(mut data) = self.project(displaced)? {
                    let files = data.get_mut("files").and_then(Value::as_array_mut).ok_or("invalid project files")?;
                    files.retain(|file| file.as_str().is_none_or(|path| canonical_path_key(path) != new));
                    data["updatedAt"] = Value::String(now_iso());
                    self.write_project_with_io(&data,io)?;
                }
            }
            if let Some(mut data) = self.project(project)? {
                let files = data.get_mut("files").and_then(Value::as_array_mut).ok_or("invalid project files")?;
                let mut seen = std::collections::HashSet::new();
                for file in files.iter_mut() {
                    if file.as_str().is_some_and(|path| canonical_path_key(path) == old) {
                        *file = Value::String(new_path.to_owned());
                    }
                }
                files.retain(|file| file.as_str().is_some_and(|path| seen.insert(canonical_path_key(path))));
                data["updatedAt"] = Value::String(now_iso());
                self.write_project_with_io(&data,io)?;
            }
        }
        let chat = match source_chat_key.as_ref() {
            Some(key) => index["chatIdByPath"][key].as_str().ok_or("invalid chat id")?.to_owned(),
            None => self.fallback_chat_id(source_project.as_deref().unwrap_or("default"), old_path)?,
        };
        if !index.get("chatIdByPath").is_some_and(Value::is_object) { index["chatIdByPath"] = json!({}); }
        let chats = index["chatIdByPath"].as_object_mut().ok_or("invalid chat map")?;
        if let Some(key) = source_chat_key { chats.remove(&key); }
        if displaced_chat.is_some() { if let Some(key) = dest_chat_key { chats.remove(&key); } }
        chats.insert(new, Value::String(chat.clone()));
        if let Some(displaced) = displaced_chat.filter(|id| id != &chat) {
            let project = dest_project.as_deref().or(source_project.as_deref()).unwrap_or("default");
            let source = self.chat_path(project, &displaced)?;
            if source.exists() {
                let trash = self.base.join(".trash");
                let target = trash.join(format!("{project}-{displaced}-{}.jsonl",now_ms() as u64));
                io.trash(&source,&trash,&target)?;
                io.forget(&mut self.sequences,(project.to_owned(),displaced))?;
            }
        }
        io.json(&self.base.join("index.json"),&index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> PathBuf {
        let mut random = [0; 12];
        getrandom::fill(&mut random).unwrap();
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        std::env::temp_dir().join(format!("project-store-{name}"))
    }

    #[test]
    fn lookup_is_read_only_and_opening_messages_materialize_on_reply() {
        let dir = sandbox();
        let mut store = NativeProjectStore::new(&dir);
        let ids = store.lookup_chat_for_file("/unsaved/example.docx").unwrap();
        assert_eq!(ids["projectId"], "default");
        assert!(!dir.exists());
        store.append_chat_message("default", "chat", &json!({"role":"user","text":"question"})).unwrap();
        assert!(!dir.exists());
        assert_eq!(store.load_chat("default", "chat", 200.0).unwrap().len(), 1);
        store.append_chat_message("default", "chat", &json!({"role":"assistant","text":"answer"})).unwrap();
        let messages = store.load_chat("default", "chat", 200.0).unwrap();
        assert_eq!(messages.iter().map(|item| item["seq"].as_f64().unwrap()).collect::<Vec<_>>(), [0.0, 1.0]);
        assert_eq!(messages[0]["text"], "question");
        assert_eq!(messages[1]["text"], "answer");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rebind_merges_existing_transcripts_and_preserves_sequence() {
        let dir = sandbox();
        let mut store = NativeProjectStore::new(&dir);
        store.append_chat_message("default", "target", &json!({"role":"assistant","text":"existing"})).unwrap();
        store.append_chat_message("default", "source", &json!({"role":"user","text":"opening"})).unwrap();
        store.rebind_chat("default", "source", "default", "target").unwrap();
        store.append_chat_message("default", "target", &json!({"role":"assistant","text":"next"})).unwrap();
        let messages = store.load_chat("default", "target", 200.0).unwrap();
        assert_eq!(messages.iter().map(|item| item["seq"].as_f64().unwrap()).collect::<Vec<_>>(), [0.0, 1.0, 2.0]);
        assert_eq!(messages[1]["text"], "opening");
        assert!(store.load_chat("default", "source", 200.0).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn moved_document_keeps_chat_identity() {
        let dir = sandbox();
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.docx");
        let new = dir.join("new.docx");
        fs::write(&old, b"document").unwrap();
        let mut store = NativeProjectStore::new(&dir);
        let before = store.resolve_chat_for_file(old.to_str().unwrap()).unwrap();
        fs::rename(&old, &new).unwrap();
        store.file_renamed(old.to_str().unwrap(), new.to_str().unwrap()).unwrap();
        let after = store.lookup_chat_for_file(new.to_str().unwrap()).unwrap();
        assert_eq!(before, after);
        assert!(store.known_file_paths().contains(&canonical_path_key(new.to_str().unwrap())));
        assert!(!store.known_file_paths().contains(&canonical_path_key(old.to_str().unwrap())));
        fs::remove_dir_all(dir).unwrap();
    }
}
