use super::*;

pub struct ChatDocumentOpen {
    pub path: Option<String>,
}

pub struct ChatAppendOwner {
    pub sender: i64,
    pub window: ProjectWindow,
    pub chat_ref: ProjectChatRef,
}

impl ChatAppendRequest {
    pub fn into_io_parts(self) -> (super::super::chat_store::ChatOperation, ChatAppendOwner) {
        let Self {
            sender,
            window,
            chat_ref,
            args,
            poster,
        } = self;
        (
            super::super::chat_store::ChatOperation::Append {
                args,
                sender: poster,
            },
            ChatAppendOwner {
                sender,
                window,
                chat_ref,
            },
        )
    }
}

impl ProjectIpc {
    pub fn finish_initial_chat_result<P>(
        mut selection: ProjectSelection,
        initial_chat_id: Option<ProjectId>,
    ) -> Result<IpcStep<P>, Thrown> {
        selection.initial_chat_id = initial_chat_id;
        reply(&selection).map(IpcStep::Done)
    }

    pub fn finish_list_chats_result<P>(
        chats: Vec<ProjectChatListItem>,
        legacy: Option<LegacyDocumentChat>,
    ) -> Result<IpcStep<P>, Thrown> {
        reply(&ProjectChatListResult { chats, legacy }).map(IpcStep::Done)
    }

    pub fn finish_open_chat_result<P>(
        &mut self,
        sender: i64,
        chat_ref: &ProjectChatRef,
        opened: Value,
    ) -> Result<IpcStep<P>, Thrown> {
        if status_of(&opened) == Some("ok") {
            self.watch(sender, chat_ref);
        }
        Ok(IpcStep::Done(Some(opened)))
    }

    pub fn finish_rename_chat_result<P>(
        &mut self,
        deps: &mut ProjectIpcDeps<'_, P>,
        chat_ref: &ProjectChatRef,
        result: ProjectWriteResult,
    ) -> Result<IpcStep<P>, Thrown> {
        let result = self.announced(
            &mut *deps.host,
            &chat_ref.project_id,
            ProjectChangedPart::Chats,
            Ok(result),
        )?;
        reply(&result).map(IpcStep::Done)
    }

    pub fn finish_chat_write_result<P>(result: ProjectWriteResult) -> Result<IpcStep<P>, Thrown> {
        reply(&result).map(IpcStep::Done)
    }

    pub fn finish_new_chat_result<P>(
        &mut self,
        sender: i64,
        project_id: ProjectId,
        chat_id: ProjectId,
    ) -> Result<IpcStep<P>, Thrown> {
        self.watch(
            sender,
            &ProjectChatRef {
                project_id,
                chat_id: chat_id.clone(),
            },
        );
        Ok(IpcStep::Done(Some(
            serde_json::json!({ "chatId": chat_id }),
        )))
    }

    pub fn finish_delete_chat_result<P>(
        &mut self,
        deps: &mut ProjectIpcDeps<'_, P>,
        sender: i64,
        chat_ref: &ProjectChatRef,
        result: ProjectWriteResult,
    ) -> Result<IpcStep<P>, Thrown> {
        if result == ProjectWriteResult::Ok {
            if self.watching.get(&sender) == Some(&key_of(&chat_ref.project_id, &chat_ref.chat_id))
            {
                self.unwatch(sender);
            }
            self.announce(
                &mut *deps.host,
                &chat_ref.project_id,
                ProjectChangedPart::Chats,
            );
        }
        reply(&result).map(IpcStep::Done)
    }

    pub fn finish_append_chat_result<P>(
        &mut self,
        deps: &mut ProjectIpcDeps<'_, P>,
        request: ChatAppendOwner,
        result: Value,
    ) -> Result<IpcStep<P>, Thrown> {
        self.forward_appended(deps);
        let ChatAppendOwner {
            sender, chat_ref, ..
        } = request;
        self.watch(sender, &chat_ref);
        if status_of(&result) == Some("saved") {
            self.announce(
                &mut *deps.host,
                &chat_ref.project_id,
                ProjectChangedPart::Chats,
            );
        }
        Ok(IpcStep::Done(Some(result)))
    }

    pub fn prepare_open_document_chat_result<P>(
        &self,
        deps: &mut ProjectIpcDeps<'_, P>,
        chat_ref: &ProjectChatRef,
        info: Option<ChatDocuments>,
    ) -> Option<ChatDocumentOpen> {
        let info = info?;
        let root = deps.guide.project_root(&chat_ref.project_id);
        let path = match (
            truthy(info.documents.last().map(String::as_str)),
            truthy(root.as_deref()),
        ) {
            (Some(latest), Some(root)) if info.last_editor != ProjectEditor::Home => {
                document_path_in(root, latest, &self.platform)
            }
            _ => None,
        };
        Some(ChatDocumentOpen { path })
    }

    pub fn finish_open_document_chat_target<P>(
        &mut self,
        deps: &mut ProjectIpcDeps<'_, P>,
        chat_ref: &ProjectChatRef,
        target: Option<i64>,
    ) -> Result<IpcStep<P>, Thrown> {
        if let Some(target) = target {
            let payload =
                serde_json::to_value(chat_ref).map_err(|error| Thrown(error.to_string()))?;
            self.deliver(
                &mut *deps.host,
                target,
                ProjectFolderEventMethod::OnOpenChat,
                payload,
            );
        }
        Ok(IpcStep::Done(None))
    }

    pub fn forward_chat_io_events<P>(&mut self, deps: &mut ProjectIpcDeps<'_, P>) {
        self.forward_appended(deps);
    }

    pub fn chat_append_io_owner_current<P>(
        &self,
        deps: &mut ProjectIpcDeps<'_, P>,
        owner: &ChatAppendOwner,
    ) -> Result<bool, Thrown> {
        Ok(Self::window_of(&mut *deps.host, owner.sender)? == owner.window)
    }
}
