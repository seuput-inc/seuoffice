use main_core::packages::electron_utils::unsaved_files::{OpenFileState, OpenFilesSourceFailed};

pub(super) fn open_files(then: Box<dyn FnOnce(Option<Result<Vec<OpenFileState>, OpenFilesSourceFailed>>)>) {
    super::super::epub_open_files::open_files(Box::new(move |result| {
        then(result.map(|answer| answer.map_err(|_| OpenFilesSourceFailed)));
    }));
}
