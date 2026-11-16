//! Excel threaded comments, persons and legacy comment mirrors.
use std::collections::{HashMap, HashSet};

use super::{
    Legacy, PackageEditor, append_rel, escape, note_ref, re, sheets, state::resolve_rel_target,
};
use serde_json::Value;

pub(super) const REL_TYPE: &str =
    "http://schemas.microsoft.com/office/2017/10/relationships/threadedComment";
const PERSON_REL: &str = "http://schemas.microsoft.com/office/2017/10/relationships/person";
const THREAD_CONTENT: &str = "application/vnd.ms-excel.threadedcomments+xml";
const PERSON_CONTENT: &str = "application/vnd.ms-excel.person+xml";
const NS: &str = "http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments";
const PERSON_PATH: &str = "xl/persons/person.xml";
const WORKBOOK_RELS: &str = "xl/_rels/workbook.xml.rels";
const CONTENT_TYPES: &str = "[Content_Types].xml";
const NOTICE: &str = "[Threaded comment]\n\nYour version of Excel allows you to read this threaded comment; however, any edits to it will get removed if the file is opened in a newer version of Excel. Learn more: https://go.microsoft.com/fwlink/?linkid=870924\n\nComment:\n    ";

pub(super) struct SyncResult {
    pub legacy: HashMap<String, Legacy>,
    pub part_removed: bool,
    pub part_path: Option<String>,
    pub rels_xml: String,
}

#[derive(Clone)]
struct Entry {
    raw: String,
    id: String,
    person: String,
    dt: String,
    done: bool,
    parent: Option<String>,
    text: String,
}

pub(super) fn person_id_for(display_name: &str) -> String {
    let mut hex = String::with_capacity(32);
    for seed in 0u32..4 {
        let mut hash = 0x811c9dc5u32 ^ seed.wrapping_mul(0x9e3779b9);
        for ch in format!("{seed}:{display_name}").chars() {
            hash = (hash ^ ch as u32).wrapping_mul(0x01000193);
        }
        hex.push_str(&format!("{hash:08X}"));
    }
    format!(
        "{{{}-{}-{}-{}-{}}}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
struct Persons {
    path: String,
    exists: bool,
    xml: String,
    ids: HashSet<String>,
}

fn unescape(text: &str) -> Result<String, String> {
    let text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    let convert = |text: &str, pattern: &str, radix| -> Result<String, String> {
        let regex = re(pattern)?;
        let mut out = String::new();
        let mut pos = 0;
        for found in regex.try_find_iter(text) {
            let found = found.map_err(|e| e.to_string())?;
            out.push_str(&text[pos..found.start()]);
            let code = u32::from_str_radix(found.get(1).unwrap_or(""), radix)
                .map_err(|e| e.to_string())?;
            let ch =
                char::from_u32(code).ok_or_else(|| "invalid XML character reference".to_owned())?;
            out.push(ch);
            pos = found.end();
        }
        out.push_str(&text[pos..]);
        Ok(out)
    };
    let text = convert(&text, r"&#(\d+);", 10)?;
    Ok(convert(&text, r"&#x([0-9a-fA-F]+);", 16)?.replace("&amp;", "&"))
}
fn attr(tag: &str, name: &str) -> Result<Option<String>, String> {
    let pattern = format!(r#"\s{}="([^"]*)""#, jscompat::regex_escape(name));
    let regex = re(&pattern)?;
    regex
        .try_exec(tag)
        .map_err(|e| e.to_string())?
        .and_then(|m| m.get(1))
        .map(unescape)
        .transpose()
}
fn xlsx_unescape(text: &str) -> Result<String, String> {
    let regex = re(r"_x([0-9A-Fa-f]{4})_")?;
    let mut out = String::new();
    let mut pos = 0;
    for found in regex.try_find_iter(text) {
        let found = found.map_err(|e| e.to_string())?;
        out.push_str(&text[pos..found.start()]);
        let code = u32::from_str_radix(found.get(1).unwrap_or(""), 16).unwrap_or(0);
        if let Some(ch) = char::from_u32(code) {
            out.push(ch);
        } else {
            out.push_str(found.as_str());
        }
        pos = found.end();
    }
    out.push_str(&text[pos..]);
    Ok(out)
}
fn legacy_comments(xml: &str) -> Result<HashMap<String, Legacy>, String> {
    let mut authors = Vec::new();
    for m in re(r"<author>([\s\S]*?)</author>")?.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        authors.push(xlsx_unescape(&unescape(m.get(1).unwrap_or(""))?)?);
    }
    let mut result = HashMap::new();
    for m in re(r"<comment\b([^>]*)>([\s\S]*?)</comment>")?.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        let a = m.get(1).unwrap_or("");
        let Some(reference) = attr(a, "ref")? else {
            continue;
        };
        let author_id = attr(a, "authorId")?
            .map(|v| jscompat::to_number(&v))
            .unwrap_or(-1.0);
        let author = if author_id >= 0.0 && author_id.fract() == 0.0 {
            authors.get(author_id as usize).cloned().unwrap_or_default()
        } else {
            String::new()
        };
        let mut text = String::new();
        for t in re(r"<t\b[^>]*>([\s\S]*?)</t>")?.try_find_iter(m.get(2).unwrap_or("")) {
            let t = t.map_err(|e| e.to_string())?;
            text.push_str(&xlsx_unescape(&unescape(t.get(1).unwrap_or(""))?)?);
        }
        result.insert(reference, Legacy { author, text });
    }
    Ok(result)
}
fn threads(xml: &str) -> Result<Vec<(String, Vec<Entry>)>, String> {
    let mut result: Vec<(String, Vec<Entry>)> = Vec::new();
    for m in
        re(r"<threadedComment\b([^>]*)(?:/>|>([\s\S]*?)</threadedComment>)")?.try_find_iter(xml)
    {
        let m = m.map_err(|e| e.to_string())?;
        let a = m.get(1).unwrap_or("");
        let Some(reference) = attr(a, "ref")? else {
            continue;
        };
        let body = m.get(2).unwrap_or("");
        let text = re(r"<text\b[^>]*>([\s\S]*?)</text>")?
            .try_exec(body)
            .map_err(|e| e.to_string())?
            .and_then(|v| v.get(1))
            .map(unescape)
            .transpose()?
            .unwrap_or_default();
        let entry = Entry {
            raw: m.as_str().into(),
            id: attr(a, "id")?.unwrap_or_default(),
            person: attr(a, "personId")?.unwrap_or_default(),
            dt: attr(a, "dT")?.unwrap_or_default(),
            done: attr(a, "done")?.as_deref() == Some("1"),
            parent: attr(a, "parentId")?,
            text,
        };
        if let Some((_, items)) = result.iter_mut().find(|(key, _)| key == &reference) {
            items.push(entry);
        } else {
            result.push((reference, vec![entry]));
        }
    }
    Ok(result)
}
fn string<'a>(v: &'a Value, field: &str) -> &'a str {
    v.get(field).and_then(Value::as_str).unwrap_or("")
}
fn replies(thread: &Value) -> &[Value] {
    thread
        .get("replies")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn mirror(text: &str, replies: &[Value]) -> String {
    let mut out = format!("{NOTICE}{text}");
    for reply in replies {
        out.push_str("\nReply:\n    ");
        out.push_str(string(reply, "text"));
    }
    out
}
fn entry_xml(
    reference: &str,
    id: &str,
    person: &str,
    dt: &str,
    text: &str,
    parent: Option<&str>,
    done: bool,
) -> String {
    format!(
        "<threadedComment ref=\"{}\" dT=\"{}\" personId=\"{}\" id=\"{}\"{}{}><text>{}</text></threadedComment>",
        escape(reference),
        escape(dt),
        escape(person),
        escape(id),
        parent
            .map(|p| format!(" parentId=\"{}\"", escape(p)))
            .unwrap_or_default(),
        if done { " done=\"1\"" } else { "" },
        escape(text)
    )
}
fn same_thread(entries: &[Entry], note: &Value, thread: &Value) -> bool {
    let Some(root) = entries.iter().position(|v| v.parent.is_none()) else {
        return false;
    };
    let r = &entries[root];
    let replies = replies(thread);
    if r.id != string(thread, "id")
        || r.person != string(thread, "personId")
        || r.dt != string(thread, "dT")
        || r.done != thread.get("done").and_then(Value::as_bool).unwrap_or(false)
        || r.text != string(note, "text")
        || entries.len() != replies.len() + 1
    {
        return false;
    }
    entries
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != root)
        .zip(replies)
        .all(|((_, entry), reply)| {
            entry.id == string(reply, "id")
                && entry.person == string(reply, "personId")
                && entry.dt == string(reply, "dT")
                && entry.text == string(reply, "text")
                && entry.parent.as_deref() == Some(string(thread, "id"))
        })
}
fn persons(pkg: &mut PackageEditor<'_>) -> Result<Persons, String> {
    let mut path = PERSON_PATH.to_owned();
    if pkg.has(WORKBOOK_RELS)? {
        let rels = pkg.read_text(WORKBOOK_RELS)?;
        for m in re(r"<Relationship\b[^>]*/?>")?.try_find_iter(&rels) {
            let m = m.map_err(|e| e.to_string())?;
            if m.as_str().contains(&format!("Type=\"{PERSON_REL}\"")) {
                if let Some(target) = attr(m.as_str(), "Target")?.filter(|v| !v.is_empty()) {
                    path = resolve_rel_target("xl/workbook.xml", &target)?;
                }
                break;
            }
        }
    }
    if !pkg.has(&path)? {
        return Ok(Persons {
            path: PERSON_PATH.into(),
            exists: false,
            xml: format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<personList xmlns=\"{NS}\"></personList>"
            ),
            ids: HashSet::new(),
        });
    }
    let xml = pkg.read_text(&path)?.as_ref().clone();
    let mut ids = HashSet::new();
    for m in re(r"<person\b([^>]*)/?>")?.try_find_iter(&xml) {
        let m = m.map_err(|e| e.to_string())?;
        if let Some(id) = attr(m.get(1).unwrap_or(""), "id")? {
            ids.insert(id);
        }
    }
    Ok(Persons {
        path,
        exists: true,
        xml,
        ids,
    })
}
fn ensure_person(persons: &mut Persons, id: &str, author: &str) -> bool {
    if !persons.ids.insert(id.to_owned()) {
        return false;
    }
    let name = escape(author);
    persons.xml = persons.xml.replacen("</personList>", &format!("<person displayName=\"{name}\" id=\"{}\" userId=\"{name}\" providerId=\"None\"/></personList>", escape(id)), 1);
    true
}
fn content_type(
    pkg: &mut PackageEditor<'_>,
    path: &str,
    kind: &str,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let xml = pkg.read_text(CONTENT_TYPES)?;
    if !xml.contains(&format!("PartName=\"/{path}\"")) {
        pkg.write(
            CONTENT_TYPES,
            xml.replacen(
                "</Types>",
                &format!("<Override PartName=\"/{path}\" ContentType=\"{kind}\"/></Types>"),
                1,
            ),
        )?;
        touched.insert(CONTENT_TYPES.into());
    }
    Ok(())
}
fn save_persons(
    pkg: &mut PackageEditor<'_>,
    persons: Persons,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    if persons.exists {
        pkg.write(&persons.path, persons.xml)?;
        touched.insert(persons.path);
        return Ok(());
    }
    pkg.add(&persons.path, persons.xml);
    touched.insert(persons.path.clone());
    let has = pkg.has(WORKBOOK_RELS)?;
    let rels = if has {
        pkg.read_text(WORKBOOK_RELS)?.as_ref().clone()
    } else {
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"></Relationships>".into()
    };
    let updated = append_rel(
        &rels,
        &sheets::next_free_relationship_id(&rels),
        PERSON_REL,
        "persons/person.xml",
    );
    if has {
        pkg.write(WORKBOOK_RELS, updated)?;
    } else {
        pkg.add(WORKBOOK_RELS, updated);
    }
    touched.insert(WORKBOOK_RELS.into());
    content_type(pkg, &persons.path, PERSON_CONTENT, touched)
}
fn write_thread(
    reference: &str,
    note: &Value,
    thread: &Value,
    people: &mut Persons,
    changed: &mut bool,
    legacy: &mut HashMap<String, Legacy>,
    written: &mut HashSet<String>,
) -> String {
    let id = string(thread, "id");
    *changed |= ensure_person(people, string(thread, "personId"), string(thread, "author"));
    let replies = replies(thread);
    for reply in replies {
        *changed |= ensure_person(people, string(reply, "personId"), string(reply, "author"));
    }
    legacy.insert(
        reference.into(),
        Legacy {
            author: format!("tc={id}"),
            text: mirror(string(note, "text"), replies),
        },
    );
    written.insert(reference.into());
    let mut xml = entry_xml(
        reference,
        id,
        string(thread, "personId"),
        string(thread, "dT"),
        string(note, "text"),
        None,
        thread.get("done").and_then(Value::as_bool).unwrap_or(false),
    );
    for reply in replies {
        xml.push_str(&entry_xml(
            reference,
            string(reply, "id"),
            string(reply, "personId"),
            string(reply, "dT"),
            string(reply, "text"),
            Some(id),
            false,
        ));
    }
    xml
}
fn replace_text(raw: &str, text: &str) -> Result<String, String> {
    let regex = re(r"<text\b[^>]*>[\s\S]*?</text>")?;
    if let Some(m) = regex.try_exec(raw).map_err(|e| e.to_string())? {
        return Ok(format!(
            "{}<text>{}</text>{}",
            &raw[..m.start()],
            escape(text),
            &raw[m.end()..]
        ));
    }
    if let Some(prefix) = raw.strip_suffix("/>") {
        return Ok(format!(
            "{prefix}><text>{}</text></threadedComment>",
            escape(text)
        ));
    }
    Ok(raw.replacen(
        "</threadedComment>",
        &format!("<text>{}</text></threadedComment>", escape(text)),
        1,
    ))
}

pub(super) fn sync(
    pkg: &mut PackageEditor<'_>,
    worksheet_path: &str,
    rels_xml: &str,
    notes: &[Value],
    existing_comments: Option<&str>,
    touched: &mut HashSet<String>,
) -> Result<SyncResult, String> {
    let mut path = None;
    for m in re(r"<Relationship\b[^>]*/?>")?.try_find_iter(rels_xml) {
        let m = m.map_err(|e| e.to_string())?;
        if m.as_str().contains(&format!("Type=\"{REL_TYPE}\"")) {
            path = attr(m.as_str(), "Target")?
                .map(|target| resolve_rel_target(worksheet_path, &target))
                .transpose()?;
            break;
        }
    }
    if let Some(p) = &path
        && !pkg.has(p)?
    {
        path = None;
    }
    let mut ordered: Vec<(String, &Value)> = Vec::new();
    for note in notes {
        let reference = note_ref(note)?;
        if let Some((_, value)) = ordered.iter_mut().find(|(key, _)| key == &reference) {
            *value = note;
        } else {
            ordered.push((reference, note));
        }
    }
    let structured = ordered
        .iter()
        .any(|(_, n)| n.get("thread").is_some_and(|v| !v.is_null()));
    if path.is_none() && !structured {
        return Ok(SyncResult {
            legacy: HashMap::new(),
            part_removed: false,
            part_path: path,
            rels_xml: rels_xml.into(),
        });
    }
    let xml = path.as_ref().map(|p| pkg.read_text(p)).transpose()?;
    let all = match &xml {
        Some(xml) => threads(xml)?,
        None => Vec::new(),
    };
    let old = existing_comments
        .map(legacy_comments)
        .transpose()?
        .unwrap_or_default();
    let mut people = persons(pkg)?;
    let mut changed = false;
    let mut legacy = HashMap::new();
    let mut written = HashSet::new();
    let mut updated = xml.as_ref().map(|v| v.as_ref().clone()).unwrap_or_else(|| format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<ThreadedComments xmlns=\"{NS}\"></ThreadedComments>"));
    for (reference, entries) in &all {
        let note = ordered
            .iter()
            .find(|(key, _)| key == reference)
            .map(|(_, n)| *n);
        let root_index = entries
            .iter()
            .position(|entry| entry.parent.is_none())
            .unwrap_or(0);
        let root = &entries[root_index];
        if note.is_none()
            || note
                .and_then(|n| n.get("thread"))
                .is_some_and(Value::is_null)
        {
            for entry in entries {
                updated = updated.replacen(&entry.raw, "", 1);
            }
            continue;
        }
        let note = note.expect("note exists");
        let prior = old.get(reference);
        if let Some(thread) = note.get("thread") {
            if same_thread(entries, note, thread) {
                legacy.insert(
                    reference.clone(),
                    prior.cloned().unwrap_or_else(|| Legacy {
                        author: format!("tc={}", root.id),
                        text: mirror(string(note, "text"), replies(thread)),
                    }),
                );
                written.insert(reference.clone());
                continue;
            }
            for (i, entry) in entries.iter().enumerate() {
                if i != root_index {
                    updated = updated.replacen(&entry.raw, "", 1);
                }
            }
            let replacement = write_thread(
                reference,
                note,
                thread,
                &mut people,
                &mut changed,
                &mut legacy,
                &mut written,
            );
            updated = updated.replacen(&root.raw, &replacement, 1);
            continue;
        }
        let displayed = entries
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        if (string(note, "text") == displayed
            || prior.is_some_and(|v| v.text == string(note, "text")))
            && prior.is_some()
        {
            legacy.insert(reference.clone(), prior.expect("checked").clone());
            continue;
        }
        if !people.ids.contains(&root.person) {
            for entry in entries {
                updated = updated.replacen(&entry.raw, "", 1);
            }
            continue;
        }
        for (i, entry) in entries.iter().enumerate() {
            let replacement = if i == root_index {
                replace_text(&entry.raw, string(note, "text"))?
            } else {
                String::new()
            };
            updated = updated.replacen(&entry.raw, &replacement, 1);
        }
        legacy.insert(
            reference.clone(),
            Legacy {
                author: format!("tc={}", root.id),
                text: format!("{NOTICE}{}", string(note, "text")),
            },
        );
    }
    let mut additions = String::new();
    for (reference, note) in &ordered {
        if written.contains(reference) {
            continue;
        }
        if let Some(thread) = note.get("thread").filter(|v| !v.is_null()) {
            additions.push_str(&write_thread(
                reference,
                note,
                thread,
                &mut people,
                &mut changed,
                &mut legacy,
                &mut written,
            ));
        }
    }
    if !additions.is_empty() {
        updated = updated.replacen(
            "</ThreadedComments>",
            &format!("{additions}</ThreadedComments>"),
            1,
        );
    }
    if changed {
        save_persons(pkg, people, touched)?;
    }
    let mut rels = rels_xml.to_owned();
    if xml.as_ref().is_some_and(|xml| **xml == updated) {
        return Ok(SyncResult {
            legacy,
            part_removed: false,
            part_path: path,
            rels_xml: rels,
        });
    }
    if !re(r"<threadedComment\b")?
        .try_test(&updated)
        .map_err(|e| e.to_string())?
    {
        if let Some(p) = &path {
            pkg.remove(p);
            touched.insert(p.clone());
        }
        return Ok(SyncResult {
            legacy,
            part_removed: path.is_some(),
            part_path: path,
            rels_xml: rels,
        });
    }
    if let Some(p) = &path {
        pkg.write(p, updated)?;
    } else {
        let mut candidate = None;
        for i in 1..10_000 {
            let p = format!("xl/threadedComments/threadedComment{i}.xml");
            if !pkg.has(&p)? {
                candidate = Some(p);
                break;
            }
        }
        let p = candidate.ok_or("No free part name for the threaded comments part.")?;
        pkg.add(&p, updated);
        rels = append_rel(
            &rels,
            &sheets::next_free_relationship_id(&rels),
            REL_TYPE,
            &format!("../{}", p.strip_prefix("xl/").unwrap_or(&p)),
        );
        content_type(pkg, &p, THREAD_CONTENT, touched)?;
        path = Some(p);
    }
    touched.insert(path.as_ref().expect("thread part exists").clone());
    Ok(SyncResult {
        legacy,
        part_removed: false,
        part_path: path,
        rels_xml: rels,
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::package::EntrySource;
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    struct Source(HashMap<String, String>);
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> {
            Ok(self.0.keys().cloned().collect())
        }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
            self.0
                .get(path)
                .cloned()
                .map(Arc::new)
                .ok_or_else(|| format!("missing {path}"))
        }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String> {
            Ok(self.0.get(path).is_some_and(|value| value.contains(needle)))
        }
        fn short_date_pattern(&mut self) -> Result<String, String> {
            Ok(String::new())
        }
        fn release_text(&mut self, _: &str) {}
    }

    fn note() -> Value {
        json!({"row":0,"column":0,"author":"Alice","text":"Root & text","thread":{
            "id":"root","personId":"alice","author":"Alice","dT":"2026-01-01T00:00:00Z","done":false,
            "replies":[{"id":"reply","personId":"bob","author":"Bob","dT":"2026-01-02T00:00:00Z","text":"Hello"}]
        }})
    }
    fn source() -> Source {
        Source(HashMap::from([(
            CONTENT_TYPES.into(),
            "<Types></Types>".into(),
        )]))
    }

    #[test]
    fn new_thread_writes_persons_relationships_and_legacy_mirror() {
        let mut source = source();
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        let result = sync(
            &mut pkg,
            "xl/worksheets/sheet1.xml",
            "<Relationships></Relationships>",
            &[note()],
            None,
            &mut touched,
        )
        .unwrap();
        let part = result.part_path.unwrap();
        let xml = pkg.read_text(&part).unwrap();
        assert!(xml.contains("Root &amp; text"));
        assert!(xml.contains("parentId=\"root\""));
        assert!(result.rels_xml.contains(REL_TYPE));
        assert!(
            pkg.read_text(PERSON_PATH)
                .unwrap()
                .contains("displayName=\"Bob\"")
        );
        assert!(
            pkg.read_text(CONTENT_TYPES)
                .unwrap()
                .contains(THREAD_CONTENT)
        );
        let legacy = result.legacy.get("A1").unwrap();
        assert_eq!(legacy.author, "tc=root");
        assert!(legacy.text.ends_with("Root & text\nReply:\n    Hello"));
    }

    #[test]
    fn untouched_thread_keeps_xml_and_original_mirror() {
        let part = "xl/threadedComments/threadedComment1.xml";
        let thread = "<ThreadedComments><threadedComment ref=\"A1\" dT=\"2026-01-01T00:00:00Z\" personId=\"alice\" id=\"root\"><text>Root &amp; text</text></threadedComment><threadedComment ref=\"A1\" dT=\"2026-01-02T00:00:00Z\" personId=\"bob\" id=\"reply\" parentId=\"root\"><text>Hello</text></threadedComment></ThreadedComments>";
        let mut source = source();
        source.0.insert(part.into(), thread.into());
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        let rels = format!(
            "<Relationships><Relationship Type=\"{REL_TYPE}\" Target=\"../threadedComments/threadedComment1.xml\"/></Relationships>"
        );
        let old = "<comments><authors><author>tc=root</author></authors><commentList><comment ref=\"A1\" authorId=\"0\"><text><t>custom mirror</t></text></comment></commentList></comments>";
        let result = sync(
            &mut pkg,
            "xl/worksheets/sheet1.xml",
            &rels,
            &[note()],
            Some(old),
            &mut touched,
        )
        .unwrap();
        assert_eq!(&**pkg.read_text(part).unwrap(), thread);
        assert!(!touched.contains(part));
        assert_eq!(result.legacy["A1"].text, "custom mirror");
    }

    #[test]
    fn explicit_plain_note_removes_thread_part() {
        let part = "xl/threadedComments/threadedComment1.xml";
        let mut source = source();
        source.0.insert(part.into(), "<ThreadedComments><threadedComment ref=\"A1\" id=\"root\" personId=\"alice\"><text>Root</text></threadedComment></ThreadedComments>".into());
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        let rels = format!(
            "<Relationships><Relationship Type=\"{REL_TYPE}\" Target=\"../threadedComments/threadedComment1.xml\"/></Relationships>"
        );
        let result = sync(
            &mut pkg,
            "xl/worksheets/sheet1.xml",
            &rels,
            &[json!({"row":0,"column":0,"text":"note","thread":null})],
            None,
            &mut touched,
        )
        .unwrap();
        assert!(result.part_removed);
        assert!(!pkg.has(part).unwrap());
        assert!(result.legacy.is_empty());
    }

    #[test]
    fn legacy_edit_folds_replies_into_root_when_person_is_known() {
        let part = "xl/threadedComments/threadedComment1.xml";
        let mut source = source();
        source.0.insert(part.into(), "<ThreadedComments><threadedComment ref=\"A1\" id=\"root\" personId=\"alice\"><text>before</text></threadedComment><threadedComment ref=\"A1\" id=\"reply\" personId=\"alice\" parentId=\"root\"><text>reply</text></threadedComment></ThreadedComments>".into());
        source.0.insert(
            PERSON_PATH.into(),
            "<personList><person id=\"alice\"/></personList>".into(),
        );
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        let rels = format!(
            "<Relationships><Relationship Type=\"{REL_TYPE}\" Target=\"../threadedComments/threadedComment1.xml\"/></Relationships>"
        );
        let result = sync(
            &mut pkg,
            "xl/worksheets/sheet1.xml",
            &rels,
            &[json!({"row":0,"column":0,"text":"new & value"})],
            None,
            &mut touched,
        )
        .unwrap();
        let updated = pkg.read_text(part).unwrap();
        assert!(updated.contains("<text>new &amp; value</text>"));
        assert!(!updated.contains("id=\"reply\""));
        assert!(result.legacy["A1"].text.ends_with("new & value"));
    }
}
