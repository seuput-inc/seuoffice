use shell_contract::editors::{PreparedSavedDocument, SavedActorRegistration, Then};
use shell_contract::ipc::{CallContext, NativeBrowserTransition, NativeSavedDocument, NativeSavedTarget, Responder};
use shell_contract::{IpcError, IpcValue, Surface};

use super::DiagramViews;
use crate::value::thrown;

pub(super) struct SavedLoad {
    pub target: NativeSavedTarget,
    pub document: Option<NativeSavedDocument>,
    pub response: Option<IpcValue>,
    pub waiting: Option<(CallContext, Responder)>,
    pub pending_path: Option<(CallContext, Responder)>,
    pub done: Option<Then<shell_contract::editors::SavedDocumentLoadOutcome>>,
    pub token: String,
    pub delivered: bool,
    pub failure: Option<String>,
    pub controls: Vec<shell_contract::runtime::CancelToken>,
    pub subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
}

struct Prepared {
    registration: std::rc::Rc<Registration>,
    staged: Option<super::state::ViewState>,
}

struct Registration {
    views: DiagramViews,
    target: NativeSavedTarget,
    identity: std::rc::Rc<()>,
    phase: std::cell::Cell<Phase>,
}

#[derive(Clone,Copy,PartialEq,Eq)]
enum Phase { Prepared, Committed, Draining, Closed, Superseded, RolledBack }

pub(super) fn prepare(views: DiagramViews, target: NativeSavedTarget) -> Result<Box<dyn PreparedSavedDocument>, IpcError> {
    if target.surface() != Surface::Diagram { return Err(thrown("The saved target is not a diagram")); }
    let path = target.path().path().to_str().ok_or_else(|| thrown("The diagram path is not valid Unicode"))?;
    if path.is_empty() { return Err(thrown("The diagram path is empty")); }
    views.with_state(|state| {
        if state.views.contains_key(&target.view()) { return Err(thrown("The reserved diagram view already has an actor")); }
        state.views.try_reserve(1).map_err(|error|thrown(error.to_string()))
    })?;
    fn add_owner(controls:&mut Vec<shell_contract::runtime::CancelToken>,owner:&shell_contract::ipc::NativeRequestOwner)->Result<(),IpcError> {
        controls.push(owner.loss_token().ok_or_else(||thrown("The saved diagram owner is unavailable"))?);
        controls.push(owner.context.renderer().ok_or_else(||thrown("The saved diagram renderer is unavailable"))?.loss_token());
        Ok(())
    }
    let mut controls=vec![target.cancel().clone(),target.pending().clone(),target.created().loss_token()];
    controls.extend_from_slice(target.additional());
    for owner in target.participants() { add_owner(&mut controls,owner)?; }
    controls.extend(target.output().loss_tokens().map_err(thrown)?);
    let mut subscriptions=Vec::with_capacity(controls.len()+1);
    for control in &controls {
        let target=target.clone();
        let core=views.0.services.core.clone();
        let instance=views.0.id;
        subscriptions.push(control.on_cancel(move || core.post(Box::new(move || {
            if let Some(views)=super::lookup(instance) { fail(&views,&target,"The saved diagram authority was released".into()); }
        }))));
    }
    let slot = SavedLoad { target:target.clone(), document:None, response:None, waiting:None, pending_path:None, done:None, token:jscompat::random_uuid(), delivered:false, failure:None, controls, subscriptions };
    let identity=std::rc::Rc::new(());
    let staged=super::state::ViewState { tracked:true, save_revision:Some(0), saved_load:Some(slot), saved_actor_identity:Some(identity.clone()), ..Default::default() };
    let registration=std::rc::Rc::new(Registration { views, target, identity, phase:std::cell::Cell::new(Phase::Prepared) });
    Ok(Box::new(Prepared { registration, staged:Some(staged) }))
}

impl PreparedSavedDocument for Prepared {
    fn with_validated(&mut self, transition:&NativeBrowserTransition<'_>, next:&mut dyn FnMut()->Result<(),IpcError>)->Result<(),IpcError> {
        let registration=&self.registration;
        let target=&registration.target;
        if registration.phase.get()!=Phase::Prepared || self.staged.is_none() { return Err(thrown("The saved diagram actor was already committed")); }
        if !transition.matches(target.reservation()) || !(transition.matches_owner(target.browser()) || transition.proposes_owner(target.browser())) {
            return Err(thrown("The saved diagram browser transition does not match"));
        }
        if self.staged.as_ref().and_then(|entry|entry.saved_load.as_ref()).is_some_and(|slot|slot.controls.iter().any(shell_contract::runtime::CancelToken::is_cancelled)) {
            return Err(thrown("The saved diagram authority was released"));
        }
        registration.views.with_state(|state| {
            if state.views.contains_key(&target.view()) { return Err(thrown("The reserved diagram view already has an actor")); }
            if state.views.capacity()==state.views.len() { return Err(thrown("The reserved diagram actor capacity was consumed")); }
            let staged=self.staged.take().ok_or_else(||thrown("The saved diagram actor was consumed"))?;
            if let Err(error)=next() { self.staged=Some(staged); return Err(error); }
            state.views.insert(target.view(),staged);
            registration.phase.set(Phase::Committed);
            Ok(())
        })
    }

    fn finish(self:Box<Self>)->std::rc::Rc<dyn SavedActorRegistration> { self.registration.clone() }
}

impl Registration {
    fn drain_closed(&self, then:Then<()>) {
        self.phase.set(Phase::Draining);
        let detached=self.views.with_state(|state| {
            let entry=state.views.get_mut(&self.target.view()).ok_or_else(||thrown("The saved diagram actor disappeared"))?;
            if !entry.saved_actor_identity.as_ref().is_some_and(|id|std::rc::Rc::ptr_eq(id,&self.identity)) {
                return Err(thrown("The saved diagram actor was replaced"));
            }
            Ok((
                entry.saved_load.take(), entry.document.take(),
                std::mem::take(&mut entry.owned_saves), std::mem::take(&mut entry.first_save_queue),
                std::mem::take(&mut entry.owned_closes), std::mem::take(&mut entry.owned_publications),
                std::mem::take(&mut entry.owned_renames), entry.copy_now.take(),
                entry.close_save.take(), entry.save.take(),
            ))
        });
        let (slot,document,saves,queue,closes,publications,renames,copy,close_save,save)=match detached {
            Ok(detached)=>detached,
            Err(error)=>{ self.phase.set(Phase::Closed); return then(Err(error)); }
        };
        if let Some(mut slot)=slot {
            if let Some((_,reply))=slot.pending_path.take() { reply.reply(Err(thrown("The diagram view was closed"))); }
            if let Some((_,reply))=slot.waiting.take() { reply.reply(Err(thrown("The diagram view was closed"))); }
            if let Some(done)=slot.done.take() { done(Err(thrown("The diagram view was closed"))); }
        }
        if let Some(document)=document { document.revoke(); }
        for (_,pending) in saves { pending.cancel_and_finish(); }
        for (_,_,reply) in queue { reply.reply(Err(thrown("The diagram view was closed"))); }
        for (_,pending) in closes { pending.cancel_and_finish(); }
        for (_,pending) in publications { pending.cancel_and_finish(); }
        for (_,done) in renames { done(Err(thrown("The diagram view was closed"))); }
        if let Some(copy)=copy { (copy.finish)(Err(main_core::shell::renderer_disposal::Failure("The diagram view was closed".into()))); }
        if let Some(waiter)=close_save { (waiter.resume)(super::state::WaitEnd::Answer(false)); }
        if let Some(waiter)=save { (waiter.resume)(super::state::WaitEnd::Answer(false)); }
        self.phase.set(Phase::Closed);
        then(Err(thrown("The saved diagram cleanup has not settled")));
    }
}

impl SavedActorRegistration for Registration {
    fn retire_closed(&self,proof:shell_contract::ipc::NativeClosureReceipt,then:Then<()>) {
        if !matches!(self.phase.get(),Phase::Committed|Phase::Closed) || !proof.matches(self.target.reservation(),self.target.view(),self.target.created()) {
            return then(Err(thrown("The saved diagram close proof does not match")));
        }
        self.drain_closed(then);
    }

    fn retire_superseded(&self,proof:shell_contract::ipc::NativeBrowserSuperseded,then:Then<()>) {
        if !matches!(self.phase.get(),Phase::Committed|Phase::Superseded) || !proof.matches(self.target.reservation(),self.target.browser()) {
            return then(Err(thrown("The saved diagram supersession proof does not match")));
        }
        let still_current=self.views.with_state(|state|state.views.get(&self.target.view())
            .and_then(|entry|entry.saved_actor_identity.as_ref()).is_some_and(|id|std::rc::Rc::ptr_eq(id,&self.identity)));
        if still_current { return then(Err(thrown("The saved diagram actor was not superseded"))); }
        self.phase.set(Phase::Superseded);
        then(Ok(()));
    }

    fn rollback(&self,then:Then<()>) {
        match self.phase.get() {
            Phase::Prepared|Phase::RolledBack|Phase::Superseded=>{ self.phase.set(Phase::RolledBack); return then(Ok(())); }
            Phase::Committed=>return then(Err(thrown("The saved diagram actor lacks its exact close proof"))),
            Phase::Draining=>return then(Err(thrown("The saved diagram cleanup is already running"))),
            Phase::Closed=>{}
        }
        self.drain_closed(then);
    }
}

fn effects(views: &DiagramViews) -> Result<shell_contract::hosts::OwnedEffectsHost, String> {
    views.0.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned()
        .ok_or_else(|| "The saved diagram effects are unavailable".into())
}

fn with_sender(views:&DiagramViews, document:&NativeSavedDocument, cx:&CallContext, effect:&mut dyn FnMut()->Result<(),String>)->Result<(),String> {
    let actual=shell_contract::ipc::NativeRequestOwner { view:cx.sender.view, context:cx.native.clone().ok_or("The saved diagram request has no native frame")? };
    let target=document.target();
    effects(views)?.with_native_saved_document_same_origin(document,&actual,&cx.cancel,&[],target.cancel(),target.pending(),&[],&mut |_|effect())
}

pub(super) fn load(views: &DiagramViews, document: NativeSavedDocument, then: Then<shell_contract::editors::SavedDocumentLoadOutcome>) {
    let target=document.target().clone();
    let view=target.view();
    let host=match effects(views) { Ok(host)=>host, Err(error)=>return then(Err(thrown(error))) };
    let owner_loss=match document.owner().loss_token() { Some(loss)=>loss, None=>return then(Err(thrown("The saved diagram frame is unavailable"))) };
    let loss_target=target.clone();
    let core=views.0.services.core.clone();
    let instance=views.0.id;
    let mut owner_subscription=Some(owner_loss.on_cancel(move || core.post(Box::new(move || {
        if let Some(views)=super::lookup(instance) { fail(&views,&loss_target,"The saved diagram frame was released".into()); }
    }))));
    let mut completion=Some(then);
    let gate=host.with_native_saved_document_input(&document, &[], target.cancel(), target.pending(), &[], &mut |_| {
        views.with_state(|state| {
            let slot=state.views.get_mut(&view).and_then(|entry|entry.saved_load.as_mut()).ok_or("The saved diagram barrier is unavailable")?;
            if !slot.target.same_target(&target) || slot.document.is_some() || slot.failure.is_some() { return Err("The saved diagram barrier was replaced".into()); }
            slot.subscriptions.push(owner_subscription.take().ok_or("The saved diagram owner lifetime was consumed")?);
            slot.document=Some(document.clone());
            slot.done=completion.take();
            Ok(())
        })
    });
    if let Err(error)=gate {
        if let Some(done)=completion { done(Err(thrown(error))); } else { fail(views,&target,error); }
        return;
    }
    if let Some(done)=completion { done(Err(thrown("The saved diagram load was not admitted"))); return; }
    let path=target.path().path().to_owned();
    let name=match path.file_name().and_then(|name|name.to_str()) {
        Some(name)=>name.to_owned(),
        None=>return fail(views, &target, "The saved diagram name is not valid Unicode".into()),
    };
    let engine=views.0.engine.clone();
    let core=views.0.services.core.clone();
    let instance=views.0.id;
    views.0.services.workers.spawn(Box::new(move || {
        let result=engine.open_diagram(&name,target.input().bytes())
            .and_then(|result|result.map_err(|failure|failure.message));
        core.post(Box::new(move || if let Some(views)=super::lookup(instance) { finish(&views, &document, result); }));
    }));
    flush(views,view);
}

fn finish(views:&DiagramViews, document:&NativeSavedDocument, parsed:Result<super::DiagramFile,String>) {
    let target=document.target();
    let parsed=match parsed { Ok(parsed)=>parsed, Err(error)=>return fail(views,target,error) };
    let path=match target.path().path().to_str() { Some(path)=>path.to_owned(), None=>return fail(views,target,"The saved diagram path is not valid Unicode".into()) };
    let token=match views.with_state(|state|state.views.get(&target.view()).and_then(|entry|entry.saved_load.as_ref()).filter(|slot|slot.target.same_target(target)).map(|slot|slot.token.clone())) {
        Some(token)=>token, None=>return,
    };
    let mut value=super::recovery_open::file_response(parsed,None);
    if let IpcValue::Object(fields)=&mut value { fields.push(("recoveryToken".into(),crate::value::string(token))); }
    let mut response=Some(value);
    let mut allowed=Some(std::collections::HashSet::from([path.clone()]));
    let mut open_path=Some(path.clone());
    let mut save_path=Some(path.clone());
    let mut clean_path=Some(path);
    let mut new_document=Some(shell_contract::runtime::RendererGeneration::new());
    let previous=views.with_state(|state|state.views.get(&target.view()).and_then(|entry|entry.document.clone()));
    let mut actual=document.owner().clone();
    if let Some(previous)=&previous { actual.context=actual.context.with_document_lease(previous.clone()); }
    let mut queued=None;
    let host=match effects(views) { Ok(host)=>host,Err(error)=>return fail(views,target,error) };
    let result=host.with_native_saved_document_transition(document,&actual,target.cancel(),&[],target.cancel(),target.pending(),&[],&mut |_,retirement| {
        views.with_state(|state| {
            let entry=state.views.get_mut(&target.view()).ok_or("The diagram view was released")?;
            let slot=entry.saved_load.as_mut().filter(|slot|slot.target.same_target(target)).ok_or("The saved diagram barrier was replaced")?;
            let value=response.take().ok_or("The saved diagram response was consumed")?;
            let successor=new_document.take().ok_or("The saved diagram successor was consumed")?;
            match (&entry.document,&previous) {
                (None,None)=>{},
                (Some(current),Some(previous)) if current.same_generation(previous)=>retirement.retire(previous)?,
                _=>return Err("The staged diagram document was replaced".into()),
            }
            slot.response=Some(value);
            entry.tracked=true;
            entry.document=Some(successor);
            entry.first_save_running=false;
            queued=Some(std::mem::take(&mut entry.first_save_queue));
            entry.open_path=open_path.take();
            entry.allowed=allowed.take();
            entry.unopened=None;
            entry.save_path=save_path.take();
            entry.clean_path=clean_path.take();
            entry.advance_save_revision();
            Ok(())
        })
    });
    if queued.is_none() { return fail(views,target,result.err().unwrap_or_else(||"The saved diagram publication was not admitted".into())); }
    if let Some(queued)=queued { for (_,_,reply) in queued { reply.reply(Err(thrown("The diagram document was replaced"))); } }
    flush(views,target.view());
}

fn fail(views:&DiagramViews,target:&NativeSavedTarget,error:String) {
    let (pending,waiting,done)=views.with_state(|state| {
        let Some(slot)=state.views.get_mut(&target.view()).and_then(|entry|entry.saved_load.as_mut()).filter(|slot|slot.target.same_target(target)) else { return (None,None,None); };
        slot.failure=Some(error.clone());
        (slot.pending_path.take(),slot.waiting.take(),slot.done.take())
    });
    if let Some((_,reply))=pending { reply.reply(Err(thrown(error.clone()))); }
    if let Some((_,reply))=waiting { reply.reply(Err(thrown(error.clone()))); }
    if let Some(done)=done { done(Err(thrown(error))); }
}

fn flush(views:&DiagramViews,view:shell_contract::ViewId) {
    let document=views.with_state(|state|state.views.get(&view).and_then(|entry|entry.saved_load.as_ref()).and_then(|slot|slot.document.clone()));
    let Some(document)=document else { return; };
    let target=document.target();
    for is_path in [true,false] {
        let cx=views.with_state(|state| {
            let slot=state.views.get(&view)?.saved_load.as_ref()?;
            if !slot.target.same_target(target) { return None; }
            if is_path { slot.pending_path.as_ref().map(|(cx,_)|cx.clone()) }
            else if slot.response.is_some() { slot.waiting.as_ref().map(|(cx,_)|cx.clone()) }
            else { None }
        });
        let Some(cx)=cx else { continue; };
        let mut path_value=if is_path { target.path().path().to_str().map(crate::value::string) } else { None };
        let mut delivered=None;
        let result=with_sender(views,&document,&cx,&mut || views.with_state(|state| {
            let slot=state.views.get_mut(&view).and_then(|entry|entry.saved_load.as_mut()).filter(|slot|slot.target.same_target(target)).ok_or("The saved diagram barrier was replaced")?;
            if is_path {
                let value=path_value.take().ok_or("The saved diagram path is not valid Unicode")?;
                let (_,reply)=slot.pending_path.take().ok_or("The saved diagram path request was released")?;
                delivered=Some((reply,value));
            } else {
                let (_,reply)=slot.waiting.take().ok_or("The saved diagram request was released")?;
                let response=slot.response.take().ok_or("The saved diagram response was released")?;
                slot.delivered=true;
                delivered=Some((reply,response));
            }
            Ok(())
        }));
        if let Some((reply,response))=delivered { reply.reply(Ok(response)); }
        else {
            let error=result.err().unwrap_or_else(||"The saved diagram response was not admitted".into());
            let reply=views.with_state(|state|state.views.get_mut(&view).and_then(|entry|entry.saved_load.as_mut())
                .filter(|slot|slot.target.same_target(target)).and_then(|slot|if is_path { slot.pending_path.take() } else { slot.waiting.take() }));
            if let Some((_,reply))=reply { reply.reply(Err(thrown(error))); }
        }
    }
}

pub(super) fn consume_pending(views:&DiagramViews,cx:&CallContext,args:crate::value::Args,reply:Responder) {
    let present=views.with_state(|state|state.views.get(&cx.sender.view).is_some_and(|entry|entry.saved_load.is_some()));
    if !present { return reply.reply(views.consume_pending(cx,args)); }
    let mut reply=Some(reply);
    let result=views.with_state(|state| {
        let slot=state.views.get_mut(&cx.sender.view).and_then(|entry|entry.saved_load.as_mut()).ok_or("The saved diagram barrier was released")?;
        if let Some(error)=&slot.failure { return Err(error.clone()); }
        if slot.pending_path.is_some() { return Err("A saved diagram path request is already pending".into()); }
        slot.pending_path=Some((cx.clone(),reply.take().ok_or("The saved diagram reply was consumed")?));
        Ok(())
    });
    if let Err(error)=result { if let Some(reply)=reply { reply.reply(Err(thrown(error))); } return; }
    flush(views,cx.sender.view);
}

pub(super) fn open_file(views:&DiagramViews,cx:&CallContext,args:crate::value::Args,reply:Responder) {
    let present=views.with_state(|state|state.views.get(&cx.sender.view).is_some_and(|entry|entry.saved_load.is_some()));
    if !present { return reply.reply(views.open_file(cx,args)); }
    let requested=args.str(0).map(str::to_owned);
    let mut reply=Some(reply);
    let result=views.with_state(|state| {
        let slot=state.views.get_mut(&cx.sender.view).and_then(|entry|entry.saved_load.as_mut()).ok_or("The saved diagram barrier was released")?;
        if let Some(error)=&slot.failure { return Err(error.clone()); }
        if requested.as_deref()!=slot.target.path().path().to_str() { return Err("The saved diagram path does not match".into()); }
        if slot.waiting.is_some() || slot.delivered { return Err("The saved diagram input was already requested".into()); }
        slot.waiting=Some((cx.clone(),reply.take().ok_or("The saved diagram reply was consumed")?));
        Ok(())
    });
    if let Err(error)=result { if let Some(reply)=reply { reply.reply(Err(thrown(error))); } return; }
    flush(views,cx.sender.view);
}

pub(super) fn acknowledge(views:&DiagramViews,cx:&CallContext,token:&str,ok:bool,error:&str)->Result<bool,IpcError> {
    let document=views.with_state(|state|state.views.get(&cx.sender.view).and_then(|entry|entry.saved_load.as_ref())
        .filter(|slot|slot.token==token).and_then(|slot|slot.document.clone()));
    let Some(document)=document else { return Ok(false); };
    let actual=shell_contract::ipc::NativeRequestOwner { view:cx.sender.view, context:cx.native.clone().ok_or_else(||thrown("The saved diagram acknowledgement has no native frame"))? };
    let target=document.target();
    let host=effects(views).map_err(thrown)?;
    let mut completed=None;
    let result=host.with_native_saved_document_same_origin(&document,&actual,&cx.cancel,&[],target.cancel(),target.pending(),&[],&mut |_| {
        views.with_state(|state| {
            let entry=state.views.get_mut(&cx.sender.view).ok_or("The diagram view was released")?;
            let slot=entry.saved_load.as_ref().filter(|slot|slot.target.same_target(target) && slot.token==token && slot.delivered).ok_or("The saved diagram acknowledgement does not match")?;
            if slot.failure.is_some() { return Err("The saved diagram load already failed".into()); }
            completed=entry.saved_load.take();
            Ok(())
        })
    });
    if completed.is_none() {
        return Err(thrown(result.err().unwrap_or_else(||"The saved diagram acknowledgement was not admitted".into())));
    }
    if let Some(mut slot)=completed { if let Some(done)=slot.done.take() { done(if ok { Ok(shell_contract::editors::SavedDocumentLoadOutcome::Applied) } else { Err(thrown(error)) }); } }
    Ok(true)
}

pub(super) fn cancel(views:&DiagramViews,view:shell_contract::ViewId) {
    let target=views.with_state(|state|state.views.get(&view).and_then(|entry|entry.saved_load.as_ref()).map(|slot|slot.target.clone()));
    if let Some(target)=target { fail(views,&target,"The diagram view was closed".into()); }
}
