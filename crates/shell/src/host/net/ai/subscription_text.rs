//! The words of the subscription sign-in: what the visible sign-in window says
//! besides the CLI's own output, and the pages the browser shows once the
//! ChatGPT sign-in has come back (`packages/ui/src/ai-subscription-login-window.ts`
//! over the `loginWindow*` and `browserPage*` strings of
//! `strings-ai-subscription/{en,ko}.ts`).

use jscompat::{JsObject, JsonValue};
use main_core::packages::i18n::{Lang, html_lang};

/// `aiSubscriptionProviderName(provider)` and `aiSubscriptionVendorName(provider)`
fn names(provider: &str) -> Option<(&'static str, &'static str)> {
    match provider {
        "claude-code" => Some(("Claude", "Anthropic")),
        "codex" => Some(("Codex", "OpenAI")),
        "chatgpt" => Some(("ChatGPT", "OpenAI")),
        _ => None,
    }
}

struct Words {
    title: &'static str,
    intro: &'static str,
    step_browser: &'static str,
    step_address: &'static str,
    step_code: &'static str,
    step_invalid: &'static str,
    closing: &'static str,
    done_title: &'static str,
    done_body: &'static str,
    failed_title: &'static str,
    failed_body: &'static str,
}

const EN: Words = Words {
    title: "{provider} sign-in",
    intro: "This is the sign-in window of the {provider} program. The English text below comes from the {provider} program.",
    step_browser: "When the {vendor} sign-in page opens in your browser, sign in and allow access. This window then closes by itself.",
    step_address: "If no browser opened, drag over the address below that starts with https:// to select it, then right-click to copy it. Paste it into your browser's address bar to open the sign-in page.",
    step_code: "If the browser shows a code after you sign in, copy the whole code (use its copy button if there is one). Right-click once inside this window to paste it, then press Enter. It is normal that the pasted code does not appear.",
    step_invalid: "If you see \"Invalid code\", the code was not copied in full. Keep this window open, copy the code again, and paste it.",
    closing: "The sign-in did not finish. When you have read the messages above, press Enter to close this window. To try again, press the sign-in button again.",
    done_title: "Connected",
    done_body: "You can close this tab and go back to the app.",
    failed_title: "The connection was not made",
    failed_body: "Go back to the app and try again.",
};

const KO: Words = Words {
    title: "{provider} 로그인",
    intro: "이 창은 {provider} 프로그램의 로그인 창입니다. 아래에 이어지는 영어 글은 {provider} 프로그램이 보여 주는 안내입니다.",
    step_browser: "브라우저에 {vendor} 로그인 화면이 열리면 로그인하고 허용을 누르세요. 끝나면 이 창은 저절로 닫힙니다.",
    step_address: "브라우저가 열리지 않으면 아래의 https://로 시작하는 주소를 마우스로 끌어 선택하고, 마우스 오른쪽 버튼을 눌러 복사하세요. 그 주소를 브라우저 주소창에 붙여 넣으면 로그인 화면이 열립니다.",
    step_code: "로그인한 뒤 브라우저에 코드가 나오면 코드 전체를 복사하세요(복사 버튼이 있으면 그것을 누르세요). 이 창 안에서 마우스 오른쪽 버튼을 한 번 눌러 붙여 넣고 Enter를 누르세요. 붙여 넣은 코드가 화면에 보이지 않는 것이 정상입니다.",
    step_invalid: "\"Invalid code\"가 나오면 코드가 덜 복사된 것입니다. 창을 닫지 말고 코드를 다시 복사해 붙여 넣으세요.",
    closing: "로그인이 끝나지 않았습니다. 위에 나온 내용을 확인했으면 Enter를 눌러 창을 닫으세요. 다시 하려면 로그인 버튼을 다시 누르세요.",
    done_title: "연결됨",
    done_body: "이 탭을 닫고 앱으로 돌아가세요.",
    failed_title: "연결하지 못했습니다",
    failed_body: "앱으로 돌아가 다시 시도하세요.",
};

fn words(lang: Lang) -> &'static Words {
    match lang {
        Lang::En => &EN,
        Lang::Ko => &KO,
    }
}

/// `format(template, { provider, vendor })`: `{name}` placeholders filled
fn fill(template: &str, provider: &str, vendor: &str) -> String {
    template
        .replace("{provider}", provider)
        .replace("{vendor}", vendor)
}

fn object(fields: &[(&str, String)]) -> JsonValue {
    let mut out = JsObject::new();
    for (key, value) in fields {
        out.set(*key, JsonValue::String(value.clone()));
    }
    JsonValue::Object(out)
}

/// `aiSubscriptionLoginWindowText(lang, provider)`: `{ title, guide, closing }`.
/// Only Claude's sign-in can hand back a code to paste, Codex takes the sign-in
/// back from the browser by itself, and ChatGPT signs in inside the app (no
/// program window, so no steps). `None` for a provider that is none of these.
pub(super) fn login_window_text(lang: Lang, provider: &str) -> Option<JsonValue> {
    let (name, vendor) = names(provider)?;
    let words = words(lang);
    let keys: &[&str] = match provider {
        "claude-code" => &["browser", "address", "code", "invalid"],
        "codex" => &["browser", "address"],
        _ => &[],
    };
    let mut guide = vec![fill(words.intro, name, vendor), String::new()];
    for (index, key) in keys.iter().enumerate() {
        let step = match *key {
            "browser" => words.step_browser,
            "address" => words.step_address,
            "code" => words.step_code,
            _ => words.step_invalid,
        };
        guide.push(format!("{}. {}", index + 1, fill(step, name, vendor)));
    }
    Some(object(&[
        ("title", fill(words.title, name, vendor)),
        ("guide", guide.join("\n")),
        ("closing", fill(words.closing, name, vendor)),
    ]))
}

/// `aiChatGptSignInPages(lang)`: `{ done, failed }`, each `{ lang, title, body }`
pub(super) fn chat_gpt_sign_in_pages(lang: Lang) -> JsonValue {
    let words = words(lang);
    let tag = html_lang(lang);
    let page = |title: &str, body: &str| {
        object(&[
            ("lang", tag.to_owned()),
            ("title", title.to_owned()),
            ("body", body.to_owned()),
        ])
    };
    let mut pages = JsObject::new();
    pages.set("done", page(words.done_title, words.done_body));
    pages.set("failed", page(words.failed_title, words.failed_body));
    JsonValue::Object(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(value: &JsonValue, key: &str) -> String {
        match value {
            JsonValue::Object(object) => match object.get(key) {
                Some(JsonValue::String(text)) => text.clone(),
                other => panic!("{key}: {other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn claude_window_numbers_its_four_steps() {
        let text = login_window_text(Lang::En, "claude-code").unwrap();
        assert_eq!(field(&text, "title"), "Claude sign-in");
        let guide = field(&text, "guide");
        assert!(guide.starts_with("This is the sign-in window of the Claude program."));
        assert!(guide.contains("\n\n1. When the Anthropic sign-in page opens"));
        assert!(guide.contains("\n4. If you see \"Invalid code\""));
    }

    #[test]
    fn codex_has_two_steps_and_chatgpt_none() {
        let codex = field(&login_window_text(Lang::Ko, "codex").unwrap(), "guide");
        assert!(codex.contains("\n2. 브라우저가 열리지 않으면"));
        assert!(!codex.contains("\n3. "));
        let chatgpt = field(&login_window_text(Lang::En, "chatgpt").unwrap(), "guide");
        assert!(chatgpt.ends_with("program.\n"));
        assert!(login_window_text(Lang::En, "other").is_none());
    }

    #[test]
    fn pages_carry_the_html_language() {
        let pages = chat_gpt_sign_in_pages(Lang::Ko);
        let done = match &pages {
            JsonValue::Object(object) => object.get("done").unwrap().clone(),
            _ => panic!(),
        };
        assert_eq!(field(&done, "lang"), "ko-KR");
        assert_eq!(field(&done, "title"), "연결됨");
    }
}
