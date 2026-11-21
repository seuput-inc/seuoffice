/// The editing commands available in a context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditFlags {
    pub can_cut: bool,
    pub can_copy: bool,
    pub can_paste: bool,
    pub can_select_all: bool,
}

impl EditFlags {
    pub fn from_dom_selection(editable: bool, selection: &str) -> Self {
        let selected = !selection.is_empty();
        Self {
            can_cut: editable && selected,
            can_copy: selected,
            can_paste: editable,
            can_select_all: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_commands_follow_the_clicked_field_and_selection() {
        for (editable, text, cut, copy, paste) in [
            (false, "", false, false, false),
            (false, "selected", false, true, false),
            (true, "", false, false, true),
            (true, " ", true, true, true),
        ] {
            let flags = EditFlags::from_dom_selection(editable, text);
            assert_eq!(
                (
                    flags.can_cut,
                    flags.can_copy,
                    flags.can_paste,
                    flags.can_select_all
                ),
                (cut, copy, paste, true),
                "editable={editable}, selection={text:?}"
            );
        }
    }
}
