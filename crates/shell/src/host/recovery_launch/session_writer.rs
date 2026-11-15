//! Fixed-path capability for native session lifecycle writes.
use std::sync::{Arc,Mutex};
use std::sync::atomic::{AtomicU8,AtomicU64,Ordering};
use main_core::packages::electron_utils::atomic_write::owned::Admission;
use main_core::shell::session_record::{session_record_path,SessionWritePlan};

#[derive(Clone)]
pub(super) struct SessionWriter {
    path:String,
    state:Arc<State>,
    store:Arc<Mutex<main_core::packages::electron_utils::recovery_store::SharedRecoveryState>>,
}
const QUIESCING:u8=1;
const RETIRED:u8=2;
const ACTIVE:u8=4;
struct State { flags:AtomicU8,recorder:AtomicU64 }
struct Active<'a>(&'a AtomicU8);
impl Drop for Active<'_> {
    fn drop(&mut self) { self.0.fetch_and(!ACTIVE,Ordering::SeqCst); }
}
impl SessionWriter {
    pub(super) fn new(store:&super::SharedStore)->Self {
        Self { path:session_record_path(&store.user_data),state:Arc::new(State { flags:AtomicU8::new(0),recorder:AtomicU64::new(0) }),store:store.shared.clone() }
    }
    pub(super) fn quiesce(&self) {
        self.state.flags.fetch_or(QUIESCING,Ordering::SeqCst);
    }
    pub(super) fn is_quiescing(&self)->bool {
        self.state.flags.load(Ordering::SeqCst) & (QUIESCING|RETIRED) != 0
    }
    pub(super) fn retire(&self) {
        self.state.flags.fetch_or(RETIRED,Ordering::SeqCst);
    }
    pub(super) fn bind_recorder(&self,plan:&SessionWritePlan)->Result<(),String> {
        if plan.file_path != self.path { return Err("The session plan belongs to another store".into()); }
        if self.is_quiescing() { return Err("The session writer is quiescing".into()); }
        match self.state.recorder.compare_exchange(0,plan.recorder_id,Ordering::SeqCst,Ordering::SeqCst) {
            Ok(_) => {},
            Err(id) if id==plan.recorder_id => {},
            Err(_) => return Err("The session recorder was replaced".into()),
        }
        if self.is_quiescing() { Err("The session writer is quiescing".into()) } else { Ok(()) }
    }
    pub(super) fn admission(&self,plan:&SessionWritePlan,final_write:bool)->Result<Admission,String> {
        if plan.file_path != self.path { return Err("The session plan belongs to another store".into()); }
        let recorder=plan.recorder_id;
        if self.state.recorder.load(Ordering::SeqCst) != recorder {
            return Err("The session plan belongs to another recorder".into());
        }
        let state=self.state.clone();
        let store=self.store.clone();
        Ok(Arc::new(move |effect| {
            let _store=&store;
            loop {
                let flags=state.flags.load(Ordering::SeqCst);
                if state.recorder.load(Ordering::SeqCst)!=recorder || flags & (RETIRED|ACTIVE)!=0 || (flags & QUIESCING!=0 && !final_write) {
                    return Err(main_core::node_fs::NodeError { code:None,message:"The native session writer is no longer active".into() });
                }
                if state.flags.compare_exchange(flags,flags|ACTIVE,Ordering::SeqCst,Ordering::SeqCst).is_ok() { break; }
            }
            let _active=Active(&state.flags);
            Ok(effect())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::shell::session_record::{create_session_recorder,SessionRecorderOptions};

    fn plan(path:&str)->SessionWritePlan {
        let mut recorder=create_session_recorder(&SessionRecorderOptions {
            user_data_dir:path.into(),write_delay_ms:None,crash_loop_window_ms:None,
        });
        recorder.prepare_start_run(1.0,false).unwrap()
    }

    #[test]
    fn lifecycle_capability_is_fixed_to_store_and_final_flush_only_after_quiesce() {
        let store=super::super::SharedStore {
            shared:Arc::new(Mutex::new(main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"))),
            user_data:"session-test\0one".into(),
        };
        let writer=SessionWriter::new(&store);
        let selected=plan("session-test\0one");
        let other=plan("session-test\0two");
        writer.bind_recorder(&selected).unwrap();
        assert!(writer.admission(&plan("session-test\0one"),true).is_err());
        assert!(writer.admission(&other,true).is_err());
        let ordinary=writer.admission(&selected,false).unwrap();
        assert!(ordinary(Box::new(||Box::new(()))).is_ok());
        let active_writer=writer.clone();
        let nested=writer.admission(&selected,true).unwrap();
        assert!(ordinary(Box::new(move || {
            active_writer.quiesce();
            assert!(nested(Box::new(||Box::new(()))).is_err());
            Box::new(())
        })).is_ok());
        assert!(ordinary(Box::new(||Box::new(()))).is_err());
        let final_write=writer.admission(&selected,true).unwrap();
        assert!(final_write(Box::new(||Box::new(()))).is_ok());
        writer.retire();
        assert!(final_write(Box::new(||Box::new(()))).is_err());
    }
}
