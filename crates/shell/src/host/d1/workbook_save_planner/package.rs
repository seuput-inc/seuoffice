//! Ordered, lazy workbook package overlay used by the mutation planner.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub(in crate::host::d1) trait EntrySource {
    fn paths(&mut self) -> Result<Vec<String>, String>;
    fn read_text(&mut self, path: &str) -> Result<Arc<String>, String>;
    fn can_patch(&mut self, path: &str) -> Result<bool, String>;
    fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String>;
    fn short_date_pattern(&mut self) -> Result<String, String>;
    fn release_text(&mut self, path: &str);
    fn has(&mut self, path: &str) -> Result<bool, String> {
        Ok(self.paths()?.iter().any(|entry| entry == path))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::host::d1) struct MutationPlan {
    pub replaced: Vec<(String, String)>,
    pub added: Vec<(String, String)>,
    pub added_binary: Vec<(String, Vec<u8>)>,
    pub removed_entries: Vec<String>,
    pub added_entries: Vec<String>,
    pub touched_entries: Vec<String>,
}

struct OrderedMap<T> {
    index: HashMap<String, usize>,
    entries: Vec<Option<(String, T)>>,
}
impl<T> Default for OrderedMap<T> {
    fn default() -> Self { Self { index: HashMap::new(), entries: Vec::new() } }
}
impl<T> OrderedMap<T> {
    fn contains(&self, key: &str) -> bool { self.index.contains_key(key) }
    fn get(&self, key: &str) -> Option<&T> {
        self.index.get(key).and_then(|index| self.entries[*index].as_ref()).map(|(_, value)| value)
    }
    fn insert(&mut self, key: String, value: T) {
        if let Some(&index) = self.index.get(&key) {
            self.entries[index] = Some((key, value));
        } else {
            self.index.insert(key.clone(), self.entries.len());
            self.entries.push(Some((key, value)));
        }
    }
    fn remove(&mut self, key: &str) -> bool {
        if let Some(index) = self.index.remove(key) { self.entries[index] = None; true } else { false }
    }
    fn keys(&self) -> impl Iterator<Item = &str> { self.entries.iter().flatten().map(|(key, _)| key.as_str()) }
    fn into_entries(self) -> impl Iterator<Item = (String, T)> { self.entries.into_iter().flatten() }
}

pub(super) struct PackageEditor<'a> {
    source: &'a mut dyn EntrySource,
    overlay: OrderedMap<Arc<String>>,
    binary: OrderedMap<Vec<u8>>,
    removed: OrderedMap<()>,
    added: OrderedMap<()>,
}
impl<'a> PackageEditor<'a> {
    pub fn new(source: &'a mut dyn EntrySource) -> Self {
        Self { source, overlay: OrderedMap::default(), binary: OrderedMap::default(), removed: OrderedMap::default(), added: OrderedMap::default() }
    }
    pub fn paths(&mut self) -> Result<Vec<String>, String> {
        let mut paths = self.source.paths()?;
        paths.retain(|path| !self.removed.contains(path));
        paths.extend(self.added.keys().map(str::to_owned));
        paths.extend(self.binary.keys().map(str::to_owned));
        Ok(paths)
    }
    pub fn has(&mut self, path: &str) -> Result<bool, String> {
        if self.removed.contains(path) { return Ok(false); }
        if self.overlay.contains(path) || self.binary.contains(path) { return Ok(true); }
        self.source.has(path)
    }
    pub fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
        if !self.removed.contains(path) {
            if let Some(text) = self.overlay.get(path) { return Ok(text.clone()); }
            if self.source.has(path)? { return self.source.read_text(path); }
        }
        Err(format!("Workbook is missing {path}."))
    }
    pub fn write(&mut self, path: &str, content: String) -> Result<(), String> {
        if self.removed.contains(path) { return Err(format!("Cannot write removed entry {path}.")); }
        self.overlay.insert(path.to_owned(), Arc::new(content));
        Ok(())
    }
    pub fn add(&mut self, path: &str, content: String) {
        self.added.insert(path.to_owned(), ());
        self.overlay.insert(path.to_owned(), Arc::new(content));
    }
    pub fn write_shared(&mut self, path: &str, content: Arc<String>) -> Result<(), String> {
        if self.removed.contains(path) { return Err(format!("Cannot write removed entry {path}.")); }
        self.overlay.insert(path.to_owned(), content);
        Ok(())
    }
    pub fn add_shared(&mut self, path: &str, content: Arc<String>) {
        self.added.insert(path.to_owned(), ());
        self.overlay.insert(path.to_owned(), content);
    }
    pub fn add_binary(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), String> {
        if self.removed.contains(path) { return Err(format!("Cannot write removed entry {path}.")); }
        self.binary.insert(path.to_owned(), bytes);
        Ok(())
    }
    pub fn remove(&mut self, path: &str) {
        self.overlay.remove(path);
        if !self.added.remove(path) { self.removed.insert(path.to_owned(), ()); }
    }
    pub fn removed_paths(&self) -> Vec<String> { self.removed.keys().map(str::to_owned).collect() }
    pub fn can_patch(&mut self, path: &str) -> Result<bool, String> {
        if self.overlay.contains(path) { Ok(true) } else { self.source.can_patch(path) }
    }
    pub fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String> {
        if let Some(text) = self.overlay.get(path) { Ok(text.contains(needle)) } else { self.source.contains_text(path, needle) }
    }
    pub fn release_source_text(&mut self, path: &str) { self.source.release_text(path); }
    pub fn short_date_pattern(&mut self) -> Result<String, String> { self.source.short_date_pattern() }
    pub fn to_plan(self, touched: HashSet<String>) -> MutationPlan {
        let mut plan = MutationPlan::default();
        for (path, text) in self.overlay.into_entries() {
            let text = Arc::try_unwrap(text).unwrap_or_else(|shared| (*shared).clone());
            if self.added.contains(&path) { plan.added.push((path, text)); }
            else { plan.replaced.push((path, text)); }
        }
        plan.added_entries.extend(self.added.keys().map(str::to_owned));
        plan.added_entries.extend(self.binary.keys().map(str::to_owned));
        plan.added_binary.extend(self.binary.into_entries());
        plan.removed_entries.extend(self.removed.keys().map(str::to_owned));
        plan.touched_entries.extend(touched);
        let js_order = |left: &String, right: &String| left.encode_utf16().cmp(right.encode_utf16());
        plan.added_entries.sort_by(js_order);
        plan.removed_entries.sort_by(js_order);
        plan.touched_entries.sort_by(js_order);
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Source;
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> { Ok(vec!["a".into(), "b".into()]) }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> { Ok(Arc::new(format!("source:{path}"))) }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> { Ok(false) }
        fn contains_text(&mut self, _: &str, needle: &str) -> Result<bool, String> { Ok(needle == "source") }
        fn release_text(&mut self, _: &str) {}
        fn short_date_pattern(&mut self) -> Result<String, String> { Ok("m/d/yyyy".into()) }
    }
    #[test]
    fn overlay_reads_earlier_writes_and_preserves_plan_order() {
        let mut source = Source;
        let mut editor = PackageEditor::new(&mut source);
        editor.write("b", "changed".into()).unwrap();
        editor.add("new", "first".into());
        editor.add("other", "second".into());
        editor.add("new", "last".into());
        editor.remove("a");
        assert_eq!(editor.read_text("b").unwrap().as_str(), "changed");
        assert!(editor.read_text("a").is_err());
        assert_eq!(editor.paths().unwrap(), vec!["b", "new", "other"]);
        let plan = editor.to_plan(HashSet::from(["b".into(), "a".into()]));
        assert_eq!(plan.replaced, vec![("b".into(), "changed".into())]);
        assert_eq!(plan.added, vec![("new".into(), "last".into()), ("other".into(), "second".into())]);
        assert_eq!(plan.removed_entries, vec!["a"]);
        assert_eq!(plan.touched_entries, vec!["a", "b"]);
    }
}
