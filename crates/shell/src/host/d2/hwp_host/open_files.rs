use main_core::packages::electron_utils::unsaved_files::{OpenFileState, OpenFilesSourceFailed};

type OpenFilesCallback = Box<dyn FnOnce(Option<Result<Vec<OpenFileState>, OpenFilesSourceFailed>>)>;

pub(super) fn open_files(then: OpenFilesCallback) {
    super::super::epub_open_files::open_files(Box::new(move |result| {
        then(result.map(|answer| answer.map_err(|_| OpenFilesSourceFailed)));
    }));
}
