//! Open-document source for the EPUB attachment guard.

use main_core::packages::electron_utils::unsaved_files::OpenFileState;
use shell_ipc_w::tabs::TabsState;

pub(super) fn open_files(then: Box<dyn FnOnce(Option<Result<Vec<OpenFileState>, ()>>)>) {
    let mut then = Some(then);
    let ran = crate::host::w::on_core_local(|provided| {
        let done = then.take().expect("callback is taken once");
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            return done(Some(Err(())));
        };
        tabs.open_documents(Box::new(move |result| {
            done(Some(
                result
                    .map(|documents| {
                        documents
                            .into_iter()
                            .map(|document| OpenFileState {
                                file_path: document.file_path,
                                dirty: document.dirty,
                            })
                            .collect()
                    })
                    .map_err(|_| ()),
            ));
        }));
    });
    if !ran {
        if let Some(done) = then {
            done(Some(Err(())));
        }
    }
}
