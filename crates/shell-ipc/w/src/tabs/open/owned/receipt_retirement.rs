use crate::tabs::host::EditorSeedRollbackStore;
use shell_contract::ipc::{NativeBrowserRetirement, NativeClosureReceipt};
use std::rc::Rc;

enum Mode {
    Rollback {
        proof: Option<NativeBrowserRetirement>,
        require_proof: bool,
    },
    Adopted(NativeClosureReceipt),
}

type Done = Box<dyn FnOnce(Result<(), String>)>;

pub fn rollback_editor_receipts(
    store: EditorSeedRollbackStore,
    proof: Option<NativeBrowserRetirement>,
    require_proof: bool,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    advance(
        store,
        Rc::new(Mode::Rollback {
            proof,
            require_proof,
        }),
        done,
    );
}

pub fn retire_adopted_editor_receipts(
    store: EditorSeedRollbackStore,
    proof: NativeClosureReceipt,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    advance(store, Rc::new(Mode::Adopted(proof)), done);
}

fn advance(store: EditorSeedRollbackStore, mode: Rc<Mode>, done: Done) {
    let receipt = store.borrow().last().cloned();
    let Some(receipt) = receipt else {
        return done(Ok(()));
    };
    match mode.as_ref() {
        Mode::Rollback {
            proof: None,
            require_proof: true,
        } => done(Err("The native closure receipt is unavailable".into())),
        Mode::Rollback {
            proof: Some(NativeBrowserRetirement::Superseded(proof)),
            ..
        } => {
            let next_receipt = receipt.clone();
            let next_mode = mode.clone();
            receipt.retire_superseded(
                proof.clone(),
                Box::new(move |result| {
                    if let Err(error) = result {
                        return done(Err(error));
                    }
                    finish_one(store, next_mode, next_receipt, done);
                }),
            );
        }
        Mode::Rollback {
            proof: Some(NativeBrowserRetirement::Closed(proof)),
            ..
        } => rollback_one(store, mode.clone(), receipt, Some(proof.clone()), done),
        Mode::Rollback {
            proof: None,
            require_proof: false,
        } => rollback_one(store, mode.clone(), receipt, None, done),
        Mode::Adopted(proof) => {
            let next_receipt = receipt.clone();
            let next_mode = mode.clone();
            receipt.retire_adopted(
                proof.clone(),
                Box::new(move |result| {
                    if let Err(error) = result {
                        return done(Err(error));
                    }
                    finish_one(store, next_mode, next_receipt, done);
                }),
            );
        }
    }
}

fn rollback_one(
    store: EditorSeedRollbackStore,
    mode: Rc<Mode>,
    receipt: Rc<dyn crate::tabs::host::EditorSeedRollback>,
    proof: Option<NativeClosureReceipt>,
    done: Done,
) {
    let next_receipt = receipt.clone();
    let next = Box::new(move |result: Result<(), String>| {
        if let Err(error) = result {
            return done(Err(error));
        }
        let finish_receipt = next_receipt.clone();
        next_receipt.rollback(Box::new(move |result| {
            if let Err(error) = result {
                return done(Err(error));
            }
            finish_one(store, mode, finish_receipt, done);
        }));
    });
    if let Some(proof) = proof {
        receipt.retire_closed(proof, next);
    } else {
        next(Ok(()));
    }
}

fn finish_one(
    store: EditorSeedRollbackStore,
    mode: Rc<Mode>,
    receipt: Rc<dyn crate::tabs::host::EditorSeedRollback>,
    done: Done,
) {
    {
        let mut entries = store.borrow_mut();
        if entries
            .last()
            .is_none_or(|last| !Rc::ptr_eq(last, &receipt))
        {
            drop(entries);
            return done(Err("The editor receipt sequence changed".into()));
        }
        entries.pop();
    }
    advance(store, mode, done);
}
