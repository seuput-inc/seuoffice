use super::*;
use crate::tabs::owned_saved::SavedAuthority;
use main_core::{docs::starred_files::write_starred_items_with, packages::electron_utils::atomic_write::{WriteAdmission, write_json_atomic_authorized}, shell::recent_files::metadata};

#[derive(Clone)]
pub(super) enum RecentEdit { Record(String), Remove(Vec<String>), Replace(String,String) }
#[derive(Clone)]
pub(super) enum StarredEdit { Toggle(String), Remove(Vec<String>), Rename(String,String), Assign(Vec<String>,Option<String>) }

impl RecentEdit {
    fn next(&self, recent: Vec<String>) -> Option<Vec<String>> {
        match self {
            Self::Record(path) => {
                if recent.first() == Some(path) { return None; }
                let mut next = Vec::with_capacity((recent.len()+1).min(RECENT_LIMIT));
                next.push(path.clone());
                next.extend(recent.into_iter().filter(|item| item != path).take(RECENT_LIMIT-1));
                Some(next)
            }
            Self::Remove(paths) => Some(recent.into_iter().filter(|path| !paths.contains(path)).collect()),
            Self::Replace(old,new) => Some(recent.into_iter().map(|path| if &path == old { new.clone() } else { path }).collect()),
        }
    }
}

impl StarredEdit {
    fn next(&self, items: &[StarredItem]) -> Option<Vec<StarredItem>> {
        match self {
            Self::Toggle(path) => Some(toggle_starred_item(items,path)),
            Self::Remove(paths) => drop_starred_items(items,paths),
            Self::Rename(old,new) => rename_starred_item(items,old,new),
            Self::Assign(paths,group) => assign_starred_group(items,paths,group.as_deref()),
        }
    }
}

fn recent_io(path: &str, edit: RecentEdit, write: &mut dyn FnMut(&str,&JsonValue)->Result<(),NodeError>) -> Result<bool,NodeError> {
    let Some(next) = edit.next(read_recent(path)) else { return Ok(false); };
    write(path,&JsonValue::Array(next.into_iter().map(JsonValue::String).collect()))?;
    Ok(true)
}
fn starred_io(path: &str, edit: StarredEdit, write: &mut dyn FnMut(&str,&JsonValue)->Result<(),NodeError>) -> Result<bool,NodeError> {
    let Some(next) = edit.next(&read_starred_items(path)) else { return Ok(false); };
    write_starred_items_with(path,&next,write)?;
    Ok(true)
}

pub(super) fn recent_sync(path: &str, edit: RecentEdit) -> Result<bool,NodeError> {
    let io_path = path.to_owned();
    metadata::run_sync(path,move || recent_io(&io_path,edit,&mut write_json_atomic))
}
pub(super) fn starred_sync(path: &str, edit: StarredEdit) -> Result<bool,NodeError> {
    let io_path = path.to_owned();
    metadata::run_sync(path,move || starred_io(&io_path,edit,&mut write_json_atomic))
}

pub(super) enum Edit { Recent(RecentEdit), Starred(StarredEdit) }
#[derive(Debug)]
pub enum Failure { Authority(String), Io(NodeError) }

#[derive(Clone)]
enum MetadataAuthority {
    Saved(SavedAuthority),
    SavedDocument {
        document: shell_contract::ipc::NativeSavedDocument,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
    },
    Native {
        operation:shell_contract::ipc::NativeInvocationOperation,
        expected:shell_contract::ipc::NativeEditorCommand,
        effects:shell_contract::hosts::OwnedEffectsHost,
    },
}
impl MetadataAuthority {
    fn with<T>(&self,action:impl FnOnce()->Result<T,String>)->Result<T,String> {
        match self {
            Self::Saved(authority)=>authority.with(action),
            Self::SavedDocument { document, participants, cancel, pending, additional, effects } => {
                let extra: Vec<_> = additional.iter().collect();
                let mut action = Some(action);
                let mut value = None;
                effects.with_native_saved_document_input(document, participants, cancel, pending, &extra, &mut |_| {
                    value = Some(action.take().ok_or("saved metadata admission was repeated")?()?);
                    Ok(())
                })?;
                value.ok_or_else(|| "saved metadata effect was not admitted".into())
            }
            Self::Native { operation,expected,effects }=>{
                let mut action=Some(action); let mut value=None;
                effects.with_native_command_preparation_effect(operation,expected,&[],operation.cancel(),operation.shutdown(),&mut || {
                    value=Some(action.take().ok_or("native metadata admission was repeated")?()?); Ok(())
                })?;
                value.ok_or_else(||"native metadata effect was not admitted".into())
            }
        }
    }
    fn check(&self)->Result<(),String> { self.with(||Ok(())) }
}
impl From<SavedAuthority> for MetadataAuthority {
    fn from(authority:SavedAuthority)->Self { Self::Saved(authority) }
}

/// Invoked on core. All IO, including native fallback descriptor writes, stays
/// in the same actual-path FIFO used by the legacy callers above.
fn owned(
    path: String, edit: Edit, authority: MetadataAuthority,
    core: Arc<dyn shell_contract::runtime::Thread>,
    done: impl FnOnce(Result<bool,Failure>) + 'static,
) {
    let done = back_to_core(core,done);
    let io_path = path.clone();
    metadata::submit(&path,move || {
        let mut refusal = None;
        let mut admit = |effect: &mut dyn FnMut()| {
            authority.with(|| { effect(); Ok(()) }).map_err(|message| {
                refusal = Some(message.clone());
                main_core::node_fs::node_error(&std::io::Error::other(message),"write",&io_path)
            })
        };
        let mut write = |path: &str, value: &JsonValue| write_json_atomic_authorized(path,value,&mut admit as &mut WriteAdmission<'_>);
        let result = match edit {
            Edit::Recent(edit) => recent_io(&io_path,edit,&mut write),
            Edit::Starred(edit) => starred_io(&io_path,edit,&mut write),
        };
        Ok(match refusal { Some(message) => Err(Failure::Authority(message)), None => result.map_err(Failure::Io) })
    },move |result| done(result.unwrap_or_else(|error| Err(Failure::Io(error)))));
}

pub struct PreparedRecent {
    path:String,
    core:Arc<dyn shell_contract::runtime::Thread>,
    authority:MetadataAuthority,
    refresh:Option<OwnedHomeRefresh>,
}

fn native_recent_matches(expected:&shell_contract::ipc::NativeEditorCommand,path:&str)->bool {
    matches!(expected,shell_contract::ipc::NativeEditorCommand::OpenFile { path:selected,.. }
        if selected.as_path()==std::path::Path::new(path))
}

impl PreparedRecent {
    pub fn record(self,path:String,done:Box<dyn FnOnce(Result<(),String>)>) {
        let Self { path:recent,core,authority,refresh } = self;
        if let MetadataAuthority::Native { expected,.. }=&authority
            && !native_recent_matches(expected,&path) {
            return done(Err("The recent file does not match the selected native open command".into()));
        }
        if let MetadataAuthority::Saved(saved)=&authority
            && let Some(destination)=&saved.existing
            && destination.path()!=std::path::Path::new(&path) {
            return done(Err("The recent file does not match the selected save destination".into()));
        }
        if let MetadataAuthority::SavedDocument { document, .. } = &authority
            && document.target().path().path() != std::path::Path::new(&path) {
            return done(Err("The recent file does not match the loaded committed document.".into()));
        }
        let checked = authority.clone();
        owned(recent,Edit::Recent(RecentEdit::Record(path)),authority,core,move |result| {
            if let Err(error) = checked.check() { done(Err(error)); return; }
            match result {
                Err(Failure::Authority(error)) => done(Err(error)),
                Ok(false) => done(Ok(())),
                Ok(true) | Err(Failure::Io(_)) => match refresh {
                    Some(refresh) => refresh(done),
                    None => done(Ok(())),
                },
            }
        });
    }
}

impl Home {
    /// Capture the actual menu recipient before any save continuation yields.
    pub fn prepare_record_recent_file_owned(
        &self,origin:&shell_contract::ipc::NativeRequestOwner,destination:&shell_contract::ipc::NativeRequestOwner,
        cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedRecent,String> {
        self.prepare_record_recent_file_many_owned(origin,destination,&[],cancel,pending,effects)
    }

    pub fn prepare_record_recent_file_many_owned(
        &self,origin:&shell_contract::ipc::NativeRequestOwner,destination:&shell_contract::ipc::NativeRequestOwner,
        participants:&[shell_contract::ipc::NativeRequestOwner],
        cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedRecent,String> {
        self.prepare_record_recent_file_many_owned_extra(origin,destination,participants,cancel,pending,&[],effects)
    }

    pub fn prepare_record_recent_file_many_owned_extra(
        &self,origin:&shell_contract::ipc::NativeRequestOwner,destination:&shell_contract::ipc::NativeRequestOwner,
        participants:&[shell_contract::ipc::NativeRequestOwner],cancel:&shell_contract::runtime::CancelToken,
        pending:&shell_contract::runtime::CancelToken,additional:&[shell_contract::runtime::CancelToken],
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedRecent,String> {
        let authority = SavedAuthority::new_extra(origin.clone(),destination.clone(),participants.to_vec(),cancel.clone(),pending.clone(),additional.to_vec(),effects.clone());
        authority.check()?;
        let refresh = self.host.prepare_recents_changed_many_owned_extra(origin,destination,participants,cancel,pending,additional,effects)?;
        Ok(PreparedRecent { path:self.recent_path.clone(),core:self.core.clone(),authority:authority.into(),refresh })
    }

    pub fn prepare_record_recent_file_recovery(
        &self,receipt:&shell_contract::ipc::RecoverySeedReceipt,receiver:&shell_contract::ipc::NativeRequestOwner,
        cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedRecent,String> {
        let authority=SavedAuthority::recovery(receipt.clone(),receiver.clone(),cancel.clone(),pending.clone(),effects.clone());
        authority.check()?;
        let refresh=self.host.prepare_recents_changed_recovery(receipt,receiver,cancel,pending,effects)?;
        Ok(PreparedRecent { path:self.recent_path.clone(),core:self.core.clone(),authority:authority.into(),refresh })
    }
    pub fn prepare_record_recent_file_existing_document(
        &self,destination:&shell_contract::ipc::NativeSaveDestination,participants:&[shell_contract::ipc::NativeRequestOwner],
        cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,additional:&[shell_contract::runtime::CancelToken],
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedRecent,String> {
        let authority=SavedAuthority::existing(destination.clone(),participants.to_vec(),cancel.clone(),pending.clone(),additional.to_vec(),effects.clone());
        authority.check()?;
        let refresh=self.host.prepare_recents_changed_existing_document(destination.binding(),participants,cancel,pending,additional,effects)?;
        Ok(PreparedRecent { path:self.recent_path.clone(),core:self.core.clone(),authority:authority.into(),refresh })
    }

    /// Bind post-open metadata to the actual loaded target and its Drive lineage.
    pub fn prepare_record_recent_file_saved_document(
        &self, document: &shell_contract::ipc::NativeSavedDocument,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken], effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<PreparedRecent, String> {
        let authority = MetadataAuthority::SavedDocument {
            document: document.clone(), participants: participants.to_vec(),
            cancel: cancel.clone(), pending: pending.clone(), additional: additional.to_vec(), effects: effects.clone(),
        };
        authority.check()?;
        let refresh = self.host.prepare_recents_changed_saved_document(document, participants, cancel, pending, additional, effects)?;
        Ok(PreparedRecent { path: self.recent_path.clone(), core: self.core.clone(), authority, refresh })
    }


    pub fn prepare_record_recent_file_native(
        &self,operation:&shell_contract::ipc::NativeInvocationOperation,
        expected:&shell_contract::ipc::NativeEditorCommand,effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedRecent,String> {
        if !matches!(expected,shell_contract::ipc::NativeEditorCommand::OpenFile { .. }) {
            return Err("The native command does not select a file".into());
        }
        let authority=MetadataAuthority::Native { operation:operation.clone(),expected:expected.clone(),effects:effects.clone() };
        authority.check()?;
        let refresh=self.host.prepare_recents_changed_native(operation,expected,effects)?;
        Ok(PreparedRecent { path:self.recent_path.clone(),core:self.core.clone(),authority,refresh })
    }
}

pub struct PreparedMovedMetadata {
    recent:String,
    starred:String,
    core:Arc<dyn shell_contract::runtime::Thread>,
    authority:SavedAuthority,
    refresh:Option<OwnedHomeRefresh>,
}

impl PreparedMovedMetadata {
    pub fn replace(self,old:String,new:String,done:Box<dyn FnOnce(Result<(),Failure>)>) {
        let Self { recent,starred,core,authority,refresh } = self;
        let next_core = core.clone();
        let next_authority = authority.clone();
        let old_starred = old.clone();
        let new_starred = new.clone();
        owned(recent,Edit::Recent(RecentEdit::Replace(old,new)),authority.into(),core,move |result| {
            if let Err(error) = result { done(Err(error)); return; }
            let checked = next_authority.clone();
            owned(starred,Edit::Starred(StarredEdit::Rename(old_starred,new_starred)),next_authority.into(),next_core,move |result| {
                if let Err(error) = result { done(Err(error)); return; }
                if let Err(error) = checked.check() { done(Err(Failure::Authority(error))); return; }
                match refresh {
                    Some(refresh) => refresh(Box::new(move |result| done(result.map_err(Failure::Authority)))),
                    None => done(Ok(())),
                }
            });
        });
    }
}

impl Home {
    pub fn prepare_moved_metadata_owned(
        &self,origin:&shell_contract::ipc::NativeRequestOwner,destination:&shell_contract::ipc::NativeRequestOwner,
        cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedMovedMetadata,String> {
        self.prepare_moved_metadata_many_owned(origin,destination,&[],cancel,pending,effects)
    }

    pub fn prepare_moved_metadata_many_owned(
        &self,origin:&shell_contract::ipc::NativeRequestOwner,destination:&shell_contract::ipc::NativeRequestOwner,
        participants:&[shell_contract::ipc::NativeRequestOwner],
        cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedMovedMetadata,String> {
        self.prepare_moved_metadata_many_owned_extra(origin,destination,participants,cancel,pending,&[],effects)
    }

    pub fn prepare_moved_metadata_many_owned_extra(
        &self,origin:&shell_contract::ipc::NativeRequestOwner,destination:&shell_contract::ipc::NativeRequestOwner,
        participants:&[shell_contract::ipc::NativeRequestOwner],cancel:&shell_contract::runtime::CancelToken,
        pending:&shell_contract::runtime::CancelToken,additional:&[shell_contract::runtime::CancelToken],
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedMovedMetadata,String> {
        let authority = SavedAuthority::new_extra(origin.clone(),destination.clone(),participants.to_vec(),cancel.clone(),pending.clone(),additional.to_vec(),effects.clone());
        authority.check()?;
        let refresh = self.host.prepare_recents_changed_many_owned_extra(origin,destination,participants,cancel,pending,additional,effects)?;
        Ok(PreparedMovedMetadata { recent:self.recent_path.clone(),starred:self.starred_path.clone(),
            core:self.core.clone(),authority,refresh })
    }

    pub fn prepare_moved_metadata_recovery(
        &self,receipt:&shell_contract::ipc::RecoverySeedReceipt,receiver:&shell_contract::ipc::NativeRequestOwner,
        cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedMovedMetadata,String> {
        let authority = SavedAuthority::recovery(receipt.clone(),receiver.clone(),cancel.clone(),pending.clone(),effects.clone());
        authority.check()?;
        let refresh = self.host.prepare_recents_changed_recovery(receipt,receiver,cancel,pending,effects)?;
        Ok(PreparedMovedMetadata { recent:self.recent_path.clone(),starred:self.starred_path.clone(),
            core:self.core.clone(),authority,refresh })
    }
    pub fn prepare_moved_metadata_existing_document(
        &self,destination:&shell_contract::ipc::NativeSaveDestination,participants:&[shell_contract::ipc::NativeRequestOwner],
        cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,additional:&[shell_contract::runtime::CancelToken],
        effects:&shell_contract::hosts::OwnedEffectsHost,
    )->Result<PreparedMovedMetadata,String> {
        let authority=SavedAuthority::existing(destination.clone(),participants.to_vec(),cancel.clone(),pending.clone(),additional.to_vec(),effects.clone());
        authority.check()?;
        let refresh=self.host.prepare_recents_changed_existing_document(destination.binding(),participants,cancel,pending,additional,effects)?;
        Ok(PreparedMovedMetadata { recent:self.recent_path.clone(),starred:self.starred_path.clone(),
            core:self.core.clone(),authority,refresh })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_record_remove_replace_order_and_limit_are_preserved() {
        assert!(RecentEdit::Record("a".into()).next(vec!["a".into(),"b".into()]).is_none());
        let current = vec!["b".into(),"a".into(),"c".into(),"a".into()];
        assert_eq!(RecentEdit::Record("a".into()).next(current).unwrap(),["a","b","c"]);
        assert_eq!(RecentEdit::Replace("a".into(),"b".into()).next(vec!["a".into(),"b".into()]).unwrap(),["b","b"]);
        assert_eq!(RecentEdit::Remove(vec!["a".into()]).next(vec!["a".into(),"b".into()]).unwrap(),["b"]);
        let current = (0..120).map(|n| n.to_string()).collect();
        assert_eq!(RecentEdit::Record("fresh".into()).next(current).unwrap().len(),100);
    }

    #[test]
    fn native_recent_file_must_be_the_exact_selected_open_path() {
        use shell_contract::{ipc::NativeEditorCommand,Surface};
        let command=NativeEditorCommand::OpenFile { surface:Surface::Docs,path:"folder/document.docx".into() };
        assert!(native_recent_matches(&command,"folder/document.docx"));
        assert!(!native_recent_matches(&command,"folder/other.docx"));
        assert!(!native_recent_matches(&NativeEditorCommand::OpenHelp { topic:None },"folder/document.docx"));
    }
}
