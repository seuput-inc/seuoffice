use main_core::markdown::image_export::ImageExportSessions;
use std::sync::{Condvar,Mutex};

pub(super) struct SessionWorker {
    state:Mutex<Option<ImageExportSessions>>,
    ready:Condvar,
}

impl SessionWorker {
    pub fn new()->Self{Self{state:Mutex::new(Some(ImageExportSessions::new())),ready:Condvar::new()}}
    pub fn run<R>(&self,work:impl FnOnce(&mut ImageExportSessions)->R)->R{
        let mut state=self.state.lock().unwrap_or_else(|poisoned|poisoned.into_inner());
        while state.is_none(){state=self.ready.wait(state).unwrap_or_else(|poisoned|poisoned.into_inner());}
        let sessions=state.take();drop(state);
        let mut lease=Lease{worker:self,sessions};
        work(lease.sessions.as_mut().expect("the image worker owns its session"))
    }
}

struct Lease<'a>{worker:&'a SessionWorker,sessions:Option<ImageExportSessions>}
impl Drop for Lease<'_>{
    fn drop(&mut self){
        let sessions=self.sessions.take();
        let mut state=self.worker.state.lock().unwrap_or_else(|poisoned|poisoned.into_inner());
        *state=sessions;drop(state);
        self.worker.ready.notify_one();
    }
}
