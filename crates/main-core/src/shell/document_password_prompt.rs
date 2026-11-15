//! Asynchronous effects of `apps/shell/src/main/document-password-prompt.ts`.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptId(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AskId(usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Prompt {
        prompt: PromptId,
        ask: AskId,
        file_name: String,
        retry: bool,
    },
    CloseWindow {
        prompt: PromptId,
    },
    Cancelled {
        ask: AskId,
    },
}

struct Prompt {
    file_name: String,
    turn: Option<usize>,
    released: bool,
    shown: bool,
    closed: bool,
    waiting: Vec<(AskId, bool)>,
}

#[derive(Default)]
pub struct DocumentPasswordPrompts {
    prompts: Vec<Prompt>,
    asks: usize,
}

impl DocumentPasswordPrompts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, file_name: &str) -> PromptId {
        let turn = self.prompts.len().checked_sub(1);
        self.prompts.push(Prompt {
            file_name: file_name.to_owned(),
            turn,
            released: false,
            shown: false,
            closed: false,
            waiting: Vec::new(),
        });
        PromptId(self.prompts.len() - 1)
    }

    fn run_ask(&mut self, id: usize, ask: AskId, retry: bool) -> Effect {
        let prompt = &mut self.prompts[id];
        if prompt.closed {
            return Effect::Cancelled { ask };
        }
        prompt.shown = true;
        Effect::Prompt {
            prompt: PromptId(id),
            ask,
            file_name: prompt.file_name.clone(),
            retry,
        }
    }

    pub fn ask(&mut self, prompt: PromptId, retry: bool) -> (AskId, Vec<Effect>) {
        let ask = AskId(self.asks);
        self.asks += 1;
        let turn = self.prompts[prompt.0].turn;
        if turn.is_none_or(|before| self.prompts[before].released) {
            (ask, vec![self.run_ask(prompt.0, ask, retry)])
        } else {
            self.prompts[prompt.0].waiting.push((ask, retry));
            (ask, Vec::new())
        }
    }
    pub fn abort(&mut self, prompt: PromptId) -> Vec<Effect> {
        let waiting = std::mem::take(&mut self.prompts[prompt.0].waiting);
        let mut effects: Vec<_> = waiting
            .into_iter()
            .map(|(ask, _)| Effect::Cancelled { ask })
            .collect();
        effects.extend(self.close(prompt));
        effects
    }

    pub fn close(&mut self, prompt: PromptId) -> Vec<Effect> {
        let id = prompt.0;
        if self.prompts[id].closed {
            return Vec::new();
        }
        self.prompts[id].closed = true;
        let mut effects = Vec::new();
        if self.prompts[id].shown {
            effects.push(Effect::CloseWindow { prompt });
        }
        self.prompts[id].released = true;
        for next in 0..self.prompts.len() {
            if self.prompts[next].turn != Some(id) {
                continue;
            }
            for (ask, retry) in std::mem::take(&mut self.prompts[next].waiting) {
                effects.push(self.run_ask(next, ask, retry));
            }
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_prompt_waits_for_close_and_retries_in_place() {
        let mut queue = DocumentPasswordPrompts::new();
        let first = queue.create("first.xlsx");
        let second = queue.create("second.pptx");
        let (a, effects) = queue.ask(first, false);
        assert_eq!(
            effects,
            vec![Effect::Prompt {
                prompt: first,
                ask: a,
                file_name: "first.xlsx".into(),
                retry: false
            }]
        );
        let (b, effects) = queue.ask(second, false);
        assert!(effects.is_empty());
        let (retry, effects) = queue.ask(first, true);
        assert_eq!(
            effects,
            vec![Effect::Prompt {
                prompt: first,
                ask: retry,
                file_name: "first.xlsx".into(),
                retry: true
            }]
        );
        assert_eq!(
            queue.close(first),
            vec![
                Effect::CloseWindow { prompt: first },
                Effect::Prompt {
                    prompt: second,
                    ask: b,
                    file_name: "second.pptx".into(),
                    retry: false
                }
            ]
        );
        assert!(queue.close(first).is_empty());
    }

    #[test]
    fn closed_queued_prompt_cannot_close_another_window() {
        let mut queue = DocumentPasswordPrompts::new();
        let first = queue.create("first.xlsx");
        let second = queue.create("second.xlsx");
        let (ask, _) = queue.ask(second, false);
        assert!(queue.close(second).is_empty());
        assert_eq!(queue.close(first), vec![Effect::Cancelled { ask }]);
    }

    #[test]
    fn owner_destruction_settles_a_queued_ask_without_closing_active_window() {
        let mut queue = DocumentPasswordPrompts::new();
        let first = queue.create("first.xlsx");
        let second = queue.create("second.xlsx");
        let (_, first_effects) = queue.ask(first, false);
        assert!(matches!(first_effects[0], Effect::Prompt { .. }));
        let (ask, _) = queue.ask(second, false);
        assert_eq!(queue.abort(second), vec![Effect::Cancelled { ask }]);
        assert_eq!(
            queue.close(first),
            vec![Effect::CloseWindow { prompt: first }]
        );
        assert!(queue.abort(second).is_empty());
    }
}
