//! The process-wide state every batch shares is one instance, whatever alias a batch
//! takes it by.

use std::any::Any;
use std::sync::{Arc, Mutex};

use main_core::packages::electron_utils::default_save_dir::SaveDirState;
use main_core::packages::electron_utils::dialog_memory::DialogMemory;
use main_core::packages::electron_utils::recovery_store::SharedRecoveryState;
use shell_contract::{Batch, Hosts};

fn need<T: Any + Send + Sync + Clone>(hosts: &Hosts, by: Batch) -> T {
    hosts.need::<T>(by).unwrap()
}

#[test]
fn process_state_is_inserted_once_for_every_batch() {
    let mut hosts = Hosts::default();
    super::install_process_state(&mut hosts);

    let recovery: shell_ipc_d1::host::RecoveryStateHost = need(&hosts, Batch::D1);
    let text_recovery: Arc<Mutex<SharedRecoveryState>> = need(&hosts, Batch::D2);
    assert!(Arc::ptr_eq(&recovery, &text_recovery));

    let memory: shell_ipc_w::SharedDialogMemory = need(&hosts, Batch::W);
    let d1_memory: Arc<Mutex<DialogMemory>> = need(&hosts, Batch::D1);
    assert!(Arc::ptr_eq(&memory, &d1_memory));

    let save_dir: shell_ipc_w::SharedSaveDirState = need(&hosts, Batch::W);
    let d1_save_dir: Arc<Mutex<SaveDirState>> = need(&hosts, Batch::D1);
    assert!(Arc::ptr_eq(&save_dir, &d1_save_dir));
}
