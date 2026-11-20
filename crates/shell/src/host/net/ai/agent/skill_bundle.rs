//! `packages/ai-provider/src/agent-session/skills.ts`: the skills an agent
//! session gets (decision 0032, spec 6.1): a copy of every enabled skill in one
//! plugin folder of the app's own. Claude Code runs with `--restricted`, which
//! keeps every user setting out, skills included, so the app copies the skill
//! folders themselves and passes the copy with `--plugin-dir`. Codex gets the
//! same copy as a folder it may read.
//!
//! Which skills go in is decided by the caller. This module only copies them:
//! one name per skill in the copy (on a shared name the winner by priority
//! keeps it, the others go in as `<name>--<source>`), and a skill used "only
//! when called" gets `disable-model-invocation: true` in the copy's front
//! matter; the source folder is never changed. The copy is kept between
//! sessions and only changed skills are copied again.
//!
//! Runs on a worker: every call blocks.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::UNIX_EPOCH;

use jscompat::node_path::host;
use jscompat::{JsObject, JsonValue, json_parse, json_stringify};
use main_core::packages::ai_provider::agent_session_skills::AGENT_SKILL_PLUGIN_NAME;
use main_core::packages::ai_provider::jsval::{as_str, object, prop, text};
use main_core::packages::ai_provider::user_skill_md::{
    EditorsText, FieldValue, IEUM_TEMPLATE_KEY, IEUM_TITLE_KEY, MAX_USER_SKILL_BYTES,
    MAX_USER_SKILL_FILES, SkillMdInput, USER_SKILL_PRIORITY, is_reserved_user_skill_name,
    parse_skill_md, render_skill_md,
};
use main_core::packages::ai_provider::user_skills_folder::{
    SkillFolderFile, SkillFolderLimits, cmp_utf16, list_skill_folder_files,
};
use main_core::packages::ai_provider::user_skills_states::is_valid_user_skill_name;
use main_core::shell::agent::agent_sessions::AgentSkillBundle;
use main_core::shell::agent::agent_skills::AgentSkillEntry;
use sha2::{Digest, Sha256};
use shell_ipc_a::agent::Fault;

const MANIFEST_FILE: &str = ".ieum-skills.json";
/// The longest name in the copy
const MAX_AGENT_SKILL_NAME: usize = 64;
const SHORT_HASH: usize = 6;
/// The only plugin root folders copied. Everything else at the root is left
/// out, including what Claude would load and run by itself (`.lsp.json`,
/// `monitors`, `hooks`, `.mcp.json`, `bin`, `settings.json`).
const PLUGIN_COPIED_DIRS: [&str; 3] = ["scripts", "references", "assets"];

/// An enabled skill to put in the copy (what the skills service lists)
struct Input {
    id: String,
    source: String,
    plugin: Option<String>,
    name: String,
    title: String,
    description: String,
    dir: String,
    invoke: String,
}

impl Input {
    fn of(skill: &JsonValue) -> Input {
        let word = |key: &str| as_str(prop(skill, key)).unwrap_or_default().to_owned();
        Input {
            id: word("id"),
            source: word("source"),
            plugin: as_str(prop(skill, "plugin")).map(str::to_owned),
            name: word("name"),
            title: word("title"),
            description: word("description"),
            dir: word("dir"),
            invoke: word("invoke"),
        }
    }

    fn rank(&self) -> usize {
        USER_SKILL_PRIORITY
            .iter()
            .position(|source| *source == self.source)
            .unwrap_or(USER_SKILL_PRIORITY.len())
    }
}

fn sha256_hex(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn short_hash(id: &str) -> String {
    sha256_hex(&[id])[..SHORT_HASH].to_owned()
}

/// `base` cut so that `-<short hash of id>` fits within the name limit
fn with_hash(base: &str, id: &str) -> String {
    let cut: String = base
        .chars()
        .take(MAX_AGENT_SKILL_NAME - SHORT_HASH - 1)
        .collect();
    let head = cut.trim_end_matches(['-', '.']);
    format!("{head}-{}", short_hash(id))
}

/// `agentSkillAltName(skill)`: the name a skill gets in the copy when another
/// enabled skill won its name: `<name>--<source>`, plugins
/// `<name>--plugin-<plugin>`. Longer than 64 characters, it is cut and ends
/// with a short hash of the identifier.
fn alt_name(skill: &Input) -> String {
    let suffix = if skill.source == "plugin" {
        format!("plugin-{}", skill.plugin.as_deref().unwrap_or_default())
    } else {
        skill.source.clone()
    };
    let mut full = String::new();
    for c in format!("{}--{suffix}", skill.name).chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            full.push(c);
        } else {
            // one per UTF-16 code unit, as `replace(/[^A-Za-z0-9._-]/g, '-')` does
            full.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    if full.len() <= MAX_AGENT_SKILL_NAME {
        full
    } else {
        with_hash(&full, &skill.id)
    }
}

/// Priority order (spec 4.4): project, app, builtin, Claude, Codex, then plugins by plugin name
fn by_priority(skills: Vec<Input>) -> Vec<Input> {
    let mut skills = skills;
    skills.sort_by(|a, b| {
        a.rank().cmp(&b.rank()).then_with(|| {
            if a.source == "plugin" {
                let (pa, pb) = (
                    a.plugin.as_deref().unwrap_or_default().to_lowercase(),
                    b.plugin.as_deref().unwrap_or_default().to_lowercase(),
                );
                cmp_utf16(&pa, &pb)
            } else {
                std::cmp::Ordering::Equal
            }
        })
    });
    skills
}

struct FolderFacts {
    files: Vec<SkillFolderFile>,
    signature: String,
    too_large: bool,
}

/// `folderFacts(dir)`: the files of a skill folder and a signature of their
/// paths, sizes and times, without reading contents. Links leading out of the
/// folder are left out, as in the skill list, so nothing is read through them.
fn folder_facts(dir: &str) -> Option<FolderFacts> {
    let listed = list_skill_folder_files(
        dir,
        Some(&SkillFolderLimits {
            files: MAX_USER_SKILL_FILES,
            bytes: MAX_USER_SKILL_BYTES,
        }),
    )?;
    let mut hash = Sha256::new();
    for file in &listed.files {
        let mtime = fs::metadata(&file.path)
            .ok()
            .and_then(|info| info.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0);
        hash.update(format!(
            "{}\0{}\0{}\n",
            file.rel,
            file.size,
            mtime.round() as i64
        ));
    }
    Some(FolderFacts {
        files: listed.files,
        signature: hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        too_large: listed.too_large,
    })
}

fn copy_files(files: &[SkillFolderFile], to: &Path) -> std::io::Result<()> {
    for file in files {
        let mut target = to.to_path_buf();
        target.extend(file.rel.split('/'));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&file.path, &target)?;
    }
    Ok(())
}

/// `pluginRootOf(skill)`: the plugin folder a plugin skill belongs to (`<plugin>/skills/<skill>`)
fn plugin_root_of(skill: &Input) -> Option<String> {
    if skill.source != "plugin" {
        return None;
    }
    let skills_dir = host::dirname(&skill.dir);
    (host::basename(&skills_dir, None).to_lowercase() == "skills")
        .then(|| host::dirname(&skills_dir))
}

/// `copyPluginSupport(pluginRoot, bundleDir)`: a plugin's supporting files to
/// the copy's root, so that a SKILL.md's `${CLAUDE_PLUGIN_ROOT}/…` finds them.
/// Links are not followed: nothing outside the plugin folder is read. The same
/// size limits as a skill apply.
fn copy_plugin_support(plugin_root: &Path, bundle_dir: &Path) {
    struct Walk<'a> {
        bundle_dir: &'a Path,
        bytes: u64,
        files: usize,
    }
    impl Walk<'_> {
        fn over(&self) -> bool {
            self.bytes > MAX_USER_SKILL_BYTES || self.files > MAX_USER_SKILL_FILES
        }

        fn walk(&mut self, current: &Path, rel: &[String]) {
            if self.over() {
                return;
            }
            let Ok(read) = fs::read_dir(current) else {
                return;
            };
            let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
            entries.sort_by(|a, b| {
                cmp_utf16(
                    &a.file_name().to_string_lossy(),
                    &b.file_name().to_string_lossy(),
                )
            });
            for entry in entries {
                if self.over() {
                    return;
                }
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_symlink() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                // at the plugin root, only the listed folders: no file and no other folder
                if rel.is_empty() && !(kind.is_dir() && PLUGIN_COPIED_DIRS.contains(&name.as_str()))
                {
                    continue;
                }
                let path = entry.path();
                let mut child = rel.to_vec();
                child.push(name);
                if kind.is_dir() {
                    self.walk(&path, &child);
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let Ok(info) = fs::metadata(&path) else {
                    continue;
                };
                if !info.is_file() {
                    continue;
                }
                if self.bytes + info.len() > MAX_USER_SKILL_BYTES
                    || self.files + 1 > MAX_USER_SKILL_FILES
                {
                    return;
                }
                let mut target = self.bundle_dir.to_path_buf();
                target.extend(&child);
                let copied = target
                    .parent()
                    .map_or(Ok(()), fs::create_dir_all)
                    .and_then(|()| fs::copy(&path, &target).map(|_| ()));
                if copied.is_err() {
                    return;
                }
                self.bytes += info.len();
                self.files += 1;
            }
        }
    }
    Walk {
        bundle_dir,
        bytes: 0,
        files: 0,
    }
    .walk(plugin_root, &[]);
}

/// `adjustSkillMd(to, name, invoke)`: make the copy's SKILL.md say what the
/// copy is: its name in the copy, and `disable-model-invocation: true` for a
/// skill used only when called (`false` for one set to "AI decides" whose file
/// says true). Other keys stay as written. `Ok(false)` when it has no front matter.
fn adjust_skill_md(to: &Path, name: &str, invoke: &str) -> std::io::Result<bool> {
    let path = to.join("SKILL.md");
    let written = String::from_utf8_lossy(&fs::read(&path)?).into_owned();
    let Ok(Some(parsed)) = parse_skill_md(&written) else {
        return Ok(false);
    };
    // a skill turned to "AI decides" says so even when its own file says only when called, so
    // Claude (which reads this key) and Codex (which is told every skill) agree
    let says_manual = parsed
        .fields
        .get("disable-model-invocation")
        .is_some_and(|value| matches!(value.to_lowercase().as_str(), "true" | "yes" | "on"));
    let fields = if invoke == "manual" {
        vec![(
            "disable-model-invocation".to_owned(),
            FieldValue::Bool(true),
        )]
    } else if says_manual {
        vec![(
            "disable-model-invocation".to_owned(),
            FieldValue::Bool(false),
        )]
    } else {
        Vec::new()
    };
    if parsed.name.as_deref() == Some(name) && fields.is_empty() {
        return Ok(true);
    }
    let rendered = render_skill_md(
        &SkillMdInput {
            name: name.to_owned(),
            description: parsed.description.clone().unwrap_or_default(),
            title: parsed.metadata.get(IEUM_TITLE_KEY).cloned(),
            template: parsed.metadata.get(IEUM_TEMPLATE_KEY).cloned(),
            editors: EditorsText::Keep,
            body: parsed.body.clone(),
            fields,
        },
        Some(&written),
    )
    .map_err(|error| std::io::Error::other(error.0))?;
    fs::write(&path, rendered)?;
    Ok(true)
}

/// What the manifest keeps of one skill of the copy
struct Kept {
    signature: String,
    copied: String,
}

fn read_manifest(path: &Path) -> HashMap<String, Kept> {
    let Ok(bytes) = fs::read(path) else {
        return HashMap::new();
    };
    let Ok(JsonValue::Object(manifest)) = json_parse(&String::from_utf8_lossy(&bytes)) else {
        return HashMap::new();
    };
    manifest
        .entries()
        .into_iter()
        .filter_map(|(name, value)| {
            let signature = as_str(prop(value, "signature"))?.to_owned();
            let copied = as_str(prop(value, "copied"))?.to_owned();
            Some((name.to_owned(), Kept { signature, copied }))
        })
        .collect()
}

/// One build at a time per bundle folder: two sessions starting together must not mix copies
static BUILDING: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);

/// `buildAgentSkillBundle(skills, bundleDir)`: bring the plugin folder in line
/// with the enabled skills: copy new and changed skills, restore a copy that
/// was changed in place, drop skills that are gone or no longer enabled.
pub fn build_agent_skill_bundle(
    skills: &[JsonValue],
    bundle_dir: &str,
) -> Result<AgentSkillBundle, Fault> {
    let key = host::resolve(&[bundle_dir]).to_lowercase();
    let lock = {
        let mut building = BUILDING.lock().unwrap_or_else(|error| error.into_inner());
        building.entry(key).or_default().clone()
    };
    let _turn = lock.lock().unwrap_or_else(|error| error.into_inner());
    build_now(skills.iter().map(Input::of).collect(), bundle_dir)
        .map_err(|error| Fault::new(error.to_string()))
}

fn build_now(input: Vec<Input>, bundle_dir: &str) -> std::io::Result<AgentSkillBundle> {
    let bundle = Path::new(bundle_dir);
    let skills_dir = bundle.join("skills");
    fs::create_dir_all(bundle.join(".claude-plugin"))?;
    fs::create_dir_all(&skills_dir)?;
    fs::write(
        bundle.join(".claude-plugin").join("plugin.json"),
        json_stringify(&object(vec![
            ("name", text(AGENT_SKILL_PLUGIN_NAME)),
            ("version", text("1.0.0")),
        ])),
    )?;
    let manifest = read_manifest(&bundle.join(MANIFEST_FILE));

    let mut entries: Vec<AgentSkillEntry> = Vec::new();
    // names given in the copy, without case
    let mut taken: HashSet<String> = HashSet::new();
    // skill names a skill has already won, without case
    let mut won: HashSet<String> = HashSet::new();
    // plugin roots whose supporting files were already copied
    let mut copied_plugins: HashSet<String> = HashSet::new();
    let mut next: Vec<(String, Kept)> = Vec::new();

    for skill in by_priority(input) {
        // never put in, from any source: not even reported
        if is_reserved_user_skill_name(&skill.name)
            || is_reserved_user_skill_name(&host::basename(&skill.dir, None))
        {
            continue;
        }
        if !is_valid_user_skill_name(&skill.name) {
            continue;
        }
        let mut name = if won.contains(&skill.name.to_lowercase()) {
            alt_name(&skill)
        } else {
            skill.name.clone()
        };
        if taken.contains(&name.to_lowercase()) {
            name = with_hash(&name, &skill.id);
        }
        if taken.contains(&name.to_lowercase()) {
            continue;
        }
        let Ok(bytes) = fs::read(Path::new(&skill.dir).join("SKILL.md")) else {
            continue;
        };
        if !matches!(
            parse_skill_md(&String::from_utf8_lossy(&bytes)),
            Ok(Some(_))
        ) {
            continue;
        }
        let Some(facts) = folder_facts(&skill.dir) else {
            continue;
        };
        if facts.too_large {
            continue;
        }
        // a plugin skill may name files beside its skills folder: ${CLAUDE_PLUGIN_ROOT}/…
        if let Some(plugin_root) = plugin_root_of(&skill)
            && copied_plugins.insert(plugin_root.clone())
        {
            copy_plugin_support(Path::new(&plugin_root), bundle);
        }
        // what the copy depends on besides the files: a new name or a changed "only when called"
        // makes a new copy even when the files are the same
        let signature = sha256_hex(&[
            skill.id.as_str(),
            "\0",
            name.as_str(),
            "\0",
            skill.invoke.as_str(),
            "\0",
            facts.signature.as_str(),
        ]);
        let to = skills_dir.join(&name);
        let before = manifest.get(&name);
        let copy = match before {
            Some(before) if before.signature == signature => folder_facts(&to.to_string_lossy()),
            _ => None,
        };
        if copy.is_none_or(|copy| Some(&copy.signature) != before.map(|kept| &kept.copied)) {
            let _ = fs::remove_dir_all(&to);
            let copied = copy_files(&facts.files, &to)
                .and_then(|()| adjust_skill_md(&to, &name, &skill.invoke));
            match copied {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    let _ = fs::remove_dir_all(&to);
                    continue;
                }
            }
        }
        let copied = folder_facts(&to.to_string_lossy());
        next.push((
            name.clone(),
            Kept {
                signature,
                copied: copied.map(|copied| copied.signature).unwrap_or_default(),
            },
        ));
        taken.insert(name.to_lowercase());
        won.insert(skill.name.to_lowercase());
        entries.push(AgentSkillEntry {
            id: skill.id,
            name,
            source: skill.source,
            plugin: skill.plugin,
            title: skill.title,
            description: skill.description,
            invoke: skill.invoke,
            skill_md: host::join(&[&to.to_string_lossy(), "SKILL.md"]),
        });
    }

    if let Ok(listed) = fs::read_dir(&skills_dir) {
        for entry in listed.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !next.iter().any(|(kept, _)| *kept == name) {
                let _ = fs::remove_dir_all(entry.path()).or_else(|_| fs::remove_file(entry.path()));
            }
        }
    }
    let mut written: JsObject<JsonValue> = JsObject::new();
    for (name, kept) in &next {
        written.set(
            name.clone(),
            object(vec![
                ("signature", text(&kept.signature)),
                ("copied", text(&kept.copied)),
            ]),
        );
    }
    fs::write(
        bundle.join(MANIFEST_FILE),
        json_stringify(&JsonValue::Object(written)),
    )?;
    Ok(AgentSkillBundle {
        dir: bundle_dir.to_owned(),
        skills: entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEMP: LazyLock<Mutex<u32>> = LazyLock::new(Mutex::default);

    fn scratch(label: &str) -> std::path::PathBuf {
        let mut count = TEMP.lock().unwrap();
        *count += 1;
        let dir = std::env::temp_dir().join(format!(
            "seuoffice-skill-bundle-{label}-{}-{}",
            std::process::id(),
            *count
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn skill_folder(root: &Path, name: &str, extra: &str) -> String {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("scripts")).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: does {name}\n{extra}---\nBody of {name}\n"),
        )
        .unwrap();
        fs::write(dir.join("scripts").join("run.py"), "print('hi')\n").unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn enabled(id: &str, source: &str, name: &str, dir: &str, invoke: &str) -> JsonValue {
        object(vec![
            ("id", text(id)),
            ("source", text(source)),
            ("name", text(name)),
            ("title", text(name)),
            ("description", text("d")),
            ("dir", text(dir)),
            ("invoke", text(invoke)),
        ])
    }

    #[test]
    fn alternate_names_name_the_source_and_stay_within_the_limit() {
        let skill = Input {
            id: "plugin:big:x".to_owned(),
            source: "plugin".to_owned(),
            plugin: Some("a plugin".to_owned()),
            name: "x".to_owned(),
            title: String::new(),
            description: String::new(),
            dir: String::new(),
            invoke: "auto".to_owned(),
        };
        assert_eq!(alt_name(&skill), "x--plugin-a-plugin");
        let long = Input {
            plugin: Some("p".repeat(100)),
            ..skill
        };
        let name = alt_name(&long);
        assert!(name.len() <= MAX_AGENT_SKILL_NAME);
        assert!(name.ends_with(&format!("-{}", short_hash("plugin:big:x"))));
    }

    #[test]
    fn the_copy_names_a_loser_by_source_marks_manual_skills_and_drops_the_gone() {
        let root = scratch("copy");
        let sources = root.join("sources");
        let one = skill_folder(&sources, "notes", "");
        let two = skill_folder(&sources.join("second"), "notes", "");
        let bundle = root.join("bundle");
        let bundle_dir = bundle.to_string_lossy().into_owned();
        let skills = vec![
            enabled("user:notes", "user", "notes", &two, "manual"),
            enabled("app:notes", "app", "notes", &one, "auto"),
        ];
        let built = build_agent_skill_bundle(&skills, &bundle_dir).unwrap();
        let names: Vec<&str> = built
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, ["notes", "notes--user"]);
        let winner = fs::read_to_string(bundle.join("skills/notes/SKILL.md")).unwrap();
        assert!(!winner.contains("disable-model-invocation"));
        let loser = fs::read_to_string(bundle.join("skills/notes--user/SKILL.md")).unwrap();
        assert!(loser.contains("name: notes--user"));
        assert!(loser.contains("disable-model-invocation: true"));
        assert!(bundle.join("skills/notes/scripts/run.py").is_file());
        assert!(bundle.join(".claude-plugin/plugin.json").is_file());

        // a second build with the user's copy no longer enabled drops it
        let again = build_agent_skill_bundle(&skills[1..], &bundle_dir).unwrap();
        assert_eq!(again.skills.len(), 1);
        assert!(!bundle.join("skills/notes--user").exists());
        // a copy changed in place is restored
        fs::write(bundle.join("skills/notes/scripts/run.py"), "tampered").unwrap();
        build_agent_skill_bundle(&skills[1..], &bundle_dir).unwrap();
        assert_eq!(
            fs::read_to_string(bundle.join("skills/notes/scripts/run.py")).unwrap(),
            "print('hi')\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reserved_names_and_names_that_are_not_valid_are_left_out() {
        let root = scratch("skip");
        let reserved = skill_folder(&root.join("a"), "seuoffice", "");
        let odd = skill_folder(&root.join("b"), "odd name", "");
        let bundle = root.join("bundle");
        let built = build_agent_skill_bundle(
            &[
                enabled("app:seuoffice", "app", "seuoffice", &reserved, "auto"),
                enabled("app:odd", "app", "odd name", &odd, "auto"),
            ],
            &bundle.to_string_lossy(),
        )
        .unwrap();
        assert!(built.skills.is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
