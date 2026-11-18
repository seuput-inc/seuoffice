use super::*;
use crate::html::text_doc::OwnedSave;

fn recovery_receiver(cx:&shell_contract::ipc::CallContext,lease:&shell_contract::runtime::RendererGeneration)->Result<shell_contract::ipc::NativeRequestOwner,shell_contract::IpcError>{
    let mut native=cx.native.clone().ok_or_else(||kit::error("the recovery renderer is not authenticated"))?;
    match native.document_lease(){
        Some(existing) if !existing.same_generation(lease)=>return Err(kit::error("the recovery document lifetime changed")),
        Some(_)=>{},
        None=>native=native.with_document_lease(lease.clone()),
    }
    Ok(shell_contract::ipc::NativeRequestOwner{view:cx.sender.view,context:native})
}

impl TextRecovery {
    pub(super) fn consume_owned_open(&self,cx:&shell_contract::ipc::CallContext)->Option<Result<IpcValue,shell_contract::IpcError>>{
        let (receipt,lease)={let views=self.views.borrow();let seed=views.get(&cx.sender.view)?.owned_open.as_ref()?;let receipt=seed.receipt.borrow().clone();(receipt,seed.lease.clone())};
        let Some(receipt)=receipt else{return Some(Err(kit::error("the recovery seed receipt is unavailable")))};
        let receiver=match recovery_receiver(cx,&lease){Ok(receiver)=>receiver,Err(error)=>return Some(Err(error))};
        let Some(effects)=self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>() else{return Some(Err(kit::error("recovery admission is unavailable")))};
        let mut recovered=None;
        let result=effects.with_recovery_open_effect(&receipt,&receiver,&cx.cancel,&mut||{
            let mut views=self.views.borrow_mut();
            let state=views.get_mut(&cx.sender.view).ok_or("the recovery view is no longer tracked")?;
            let seed=state.owned_open.as_mut().ok_or("the recovery seed was released")?;
            let value=seed.recovered.take().ok_or("the recovery seed was already consumed")?;
            let token=seed.token.clone();state.asked=true;
            recovered=Some(match value{
                Some(RecoveredText{text,original_path:Some(path)})=>obj([("recoveryToken",token.into_ipc()),("text",text.into_ipc()),("originalPath",path.into_ipc())]),
                Some(RecoveredText{text,original_path:None})=>obj([("recoveryToken",token.into_ipc()),("text",text.into_ipc())]),
                None=>obj([("recoveryToken",token.into_ipc())]),
            });
            Ok(())
        });
        if let Err(error)=result{return Some(Err(kit::error(error)))}
        let Some(loss)=receiver.loss_token() else{return Some(Err(kit::error("the recovery receiver has no native loss signal")))};
        let core=self.services.core.clone();let loss_receipt=receipt.clone();
        let subscription=loss.on_cancel(move||core.post(Box::new(move||{
            if let Ok(deferred)=loss_receipt.fail_after_loss("the recovery renderer was lost before applying its seed".into()){deferred.notify();}
        })));
        if let Some(seed)=self.views.borrow_mut().get_mut(&cx.sender.view).and_then(|state|state.owned_open.as_mut()){
            seed.loss_subscription=Some(subscription);
        }
        Some(recovered.ok_or_else(||kit::error("recovery seed was not admitted")))
    }

    pub(super) fn settle_owned_open(&self,cx:&shell_contract::ipc::CallContext,token:&str,result:Result<(),String>)->Result<(),shell_contract::IpcError>{
        let (receipt,lease)={
            let views=self.views.borrow();
            let seed=views.get(&cx.sender.view).and_then(|state|state.owned_open.as_ref()).ok_or_else(||kit::error("the recovery seed is unavailable"))?;
            if seed.token!=token || seed.recovered.is_some(){return Err(kit::error("the recovery acknowledgment does not match its applied seed"))}
            let receipt=seed.receipt.borrow().clone().ok_or_else(||kit::error("the recovery seed receipt is unavailable"))?;
            (receipt,seed.lease.clone())
        };
        let receiver=recovery_receiver(cx,&lease)?;
        let effects=self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().ok_or_else(||kit::error("recovery admission is unavailable"))?;
        effects.settle_recovery_seed(&receipt,&receiver,&cx.cancel,result).map_err(kit::error)
    }

    pub(super) fn fail_owned_open_after_loss(&self,view:ViewId,error:&str){
        let receipt=self.views.borrow().get(&view).and_then(|state|state.owned_open.as_ref())
            .and_then(|seed|seed.receipt.borrow().clone());
        if let Some(receipt)=receipt{if let Ok(deferred)=receipt.fail_after_loss(error.to_owned()){deferred.notify();}}
    }
}

pub(crate) struct RecoverySeedSpec {
    pub target:shell_contract::ipc::RecoveryOpenTarget,
    pub identity:Option<RecoveryIdentity>,
    pub untitled:bool,
    pub title:Option<String>,
    pub save_as_default:Option<String>,
    pub recovered:Option<RecoveredText>,
    pub lease:Option<shell_contract::runtime::RendererGeneration>,
}

impl TextRecovery {
    pub(crate) fn commit_recovery_seed(
        &self,spec:&mut Option<RecoverySeedSpec>,
        grant:&mut dyn FnMut()->Result<(),String>,
        validate:&mut dyn FnMut()->Result<(),String>,
        commit:&mut dyn FnMut(),
    )->Result<bool,String>{
        let Some(seed)=spec.as_ref() else{return Err("the recovery seed was already committed".into())};
        let lease=seed.lease.clone().ok_or("the recovery document lease was not prepared")?;
        let data=self.services.paths.path(PathName::UserData).to_string_lossy().into_owned();
        if self.saved_reservations.get() != 0 {
            self.views.try_borrow_mut().map_err(|_| "the recovery tracking map is busy")?
                .try_reserve(self.saved_reservations.get().saturating_add(1)).map_err(|error|error.to_string())?;
        }
        if let Some(identity)=&seed.identity{main_core::packages::electron_utils::recovery_store::recovery_copy_path(&data,identity).map_err(|error|error.message)?;}
        let shared=match self.shared.try_lock(){
            Ok(shared)=>shared,Err(std::sync::TryLockError::Poisoned(error))=>error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock)=>return Ok(false),
        };
        let effects=self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().ok_or("recovery admission is unavailable")?;
        let target=seed.target.clone();let now=(self.host.now_ms)();
        let data_dir=||data.clone();let now_ms=||now;let random_uuid=jscompat::random_uuid;
        let store=create_recovery_store(RecoveryStoreOptions{user_data_dir:&data_dir,now_ms:&now_ms,random_uuid:&random_uuid},&shared);
        let receipt=Rc::new(RefCell::new(None));let stored=receipt.clone();
        let accepted=effects.commit_recovery_seed(&target,&mut||{
            validate()?;
            grant()?;
            let seed=spec.take().expect("the validated recovery seed is consumed once");
            if let Some(identity)=&seed.identity{
                store.clear_discarded(identity).expect("the recovery identity path was validated before admission");
            }
            commit();
            let mut views=self.views.borrow_mut();let state=views.entry(target.view()).or_default();
            state.owns=seed.recovered.is_some();state.decision=None;state.untitled_seed=None;
            state.untitled=if seed.untitled{seed.identity}else{None};
            state.title=seed.title;state.save_as_default_path=seed.save_as_default;
            state.owned_open=Some(OwnedOpenSeed{receipt:stored.clone(),token:format!("recovery:{}",kit::unique()),lease:lease.clone(),loss_subscription:None,recovered:Some(seed.recovered)});
            Ok(())
        })?;
        *receipt.borrow_mut()=Some(accepted);
        Ok(true)
    }

    pub(crate) fn after_recovery_store_available(&self,then:impl FnOnce()+'static){self.after_store_available(then)}
}

impl TextRecovery {
    pub(super) fn try_owned_store_state<R>(&self,request:&OwnedSave,action:impl FnOnce(&RecoveryStore<'_>)->R)->Result<Option<R>,String>{
        let user_data=self.services.paths.path(PathName::UserData).to_string_lossy().into_owned();
        let now=(self.host.now_ms)();let mut action=Some(action);let mut result=None;
        request.admit(&mut||{
            let shared=match self.shared.try_lock(){
                Ok(shared)=>shared,
                Err(std::sync::TryLockError::Poisoned(error))=>error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock)=>return Ok(()),
            };
            let user_data_dir=||user_data.clone();let now_ms=||now;let random_uuid=jscompat::random_uuid;
            let store=create_recovery_store(RecoveryStoreOptions{user_data_dir:&user_data_dir,now_ms:&now_ms,random_uuid:&random_uuid},&shared);
            let action=action.take().ok_or("recovery state effect was admitted more than once")?;
            result=Some(action(&store));Ok(())
        })?;
        Ok(result)
    }

    pub(super) fn after_store_available(&self,then:impl FnOnce()+'static){
        let shared=self.shared.clone();
        kit::on_worker(&self.services,move||drop(shared.lock().unwrap_or_else(|poisoned|poisoned.into_inner())),move|()|then());
    }

    pub(crate) fn finish_owned(self:&Rc<Self>,view:ViewId,identity:Option<RecoveryIdentity>,reason:RecoveryDeleteReason,request:OwnedSave,then:impl FnOnce(Result<(),String>)+'static){
        let Some(key)=identity.as_ref()else{return self.after_delete_owned(view,identity,reason,request,then)};
        let plan=match self.try_owned_store_state(&request,|store|store.prepare_delete(key,reason)){
            Err(error)=>return then(Err(error)),
            Ok(None)=>{
                let me=Rc::clone(self);
                return self.after_store_available(move||me.finish_owned(view,identity,reason,request,then));
            },
            Ok(Some(Err(error)))=>{
                self.quietly("delete",Err::<(),_>(error));
                return self.after_delete_owned(view,identity,reason,request,then);
            },
            Ok(Some(Ok(None)))=>return self.after_delete_owned(view,identity,reason,request,then),
            Ok(Some(Ok(Some(plan))))=>plan,
        };
        let worker=request.clone();let me=Rc::clone(self);let kind=self.host.kind;
        kit::on_worker(&self.services,move||{
            let mut denied=None;
            let result=plan.remove_authorized(&mut|effect|{
                worker.admit(&mut||{effect();Ok(())}).map_err(|message|{
                    denied=Some(message.clone());main_core::node_fs::NodeError{code:None,message}
                })
            });
            if let Some(error)=denied{return Err(error)}
            if let Err(error)=result{eprintln!("[{kind}] recovery delete failed: {error:?}");}
            Ok(())
        },move|deleted|{
            if let Err(error)=deleted{return then(Err(error))}
            me.after_delete_owned(view,identity,reason,request,then);
        });
    }

    fn after_delete_owned(self:&Rc<Self>,view:ViewId,identity:Option<RecoveryIdentity>,reason:RecoveryDeleteReason,request:OwnedSave,then:impl FnOnce(Result<(),String>)+'static){
        let mut resume=false;
        let result=request.admit(&mut||{
            if let Some(identity)=&identity{
                for (id,state) in self.views.borrow_mut().iter_mut(){
                    if *id==view{state.drop_copy();}else if self.shows_copy(*id,state,identity){state.drop_copy();resume=true;}
                }
            }else if reason==RecoveryDeleteReason::Saved{if let Some(state)=self.views.borrow_mut().get_mut(&view){state.last_text=None;state.owns=false;}}
            if reason==RecoveryDeleteReason::Saved && (self.host.save_path)(view).is_some_and(|path|!path.is_empty()){
                if let Some(state)=self.views.borrow_mut().get_mut(&view){state.untitled=None;state.save_as_default_path=None;}
            }
            Ok(())
        });
        if let Err(error)=result{return then(Err(error))}
        if reason!=RecoveryDeleteReason::DontSave||!resume{return then(Ok(()))}
        let Some(identity)=identity else{return then(Ok(()))};
        self.clear_discarded_owned(identity,request,then);
    }

    fn clear_discarded_owned(self:&Rc<Self>,identity:RecoveryIdentity,request:OwnedSave,then:impl FnOnce(Result<(),String>)+'static){
        match self.try_owned_store_state(&request,|store|store.clear_discarded(&identity)){
            Err(error)=>then(Err(error)),
            Ok(Some(result))=>{self.quietly("resume",result);then(Ok(()));},
            Ok(None)=>{
                let me=Rc::clone(self);
                self.after_store_available(move||me.clear_discarded_owned(identity,request,then));
            }
        }
    }
}
