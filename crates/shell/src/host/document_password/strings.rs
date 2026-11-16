use shell_contract::IpcValue;

pub(super) fn state(file_name: &str, retry: bool, lang: &str) -> IpcValue {
    let ko = lang == "ko";
    let strings = if ko {
        [
            "비밀번호 입력",
            "이 파일은 암호로 보호되어 있습니다. 열기 위한 비밀번호를 입력하세요:",
            "비밀번호가 올바르지 않습니다. 다시 시도하세요.",
            "확인",
            "취소",
            "비밀번호 확인 중…",
            "암호",
            "열기 암호 입력",
            "암호 표시",
            "암호 숨기기",
        ]
    } else {
        [
            "Enter Password",
            "This file is password-protected. Enter the password to open it:",
            "Incorrect password. Please try again.",
            "OK",
            "Cancel",
            "Verifying password…",
            "Password",
            "Enter the open password",
            "Show password",
            "Hide password",
        ]
    };
    let names = [
        "title",
        "prompt",
        "retryPrompt",
        "ok",
        "cancel",
        "verifying",
        "label",
        "placeholder",
        "show",
        "hide",
    ];
    IpcValue::Object(vec![
        ("fileName".into(), IpcValue::String(file_name.to_owned())),
        ("retry".into(), IpcValue::Bool(retry)),
        ("busy".into(), IpcValue::Bool(false)),
        ("lang".into(), IpcValue::String(lang.to_owned())),
        (
            "strings".into(),
            IpcValue::Object(
                names
                    .into_iter()
                    .zip(strings)
                    .map(|(name, value)| (name.into(), IpcValue::String(value.into())))
                    .collect(),
            ),
        ),
    ])
}
