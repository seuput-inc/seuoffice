//! GitHub skill addresses and immutable revision resolution. HTTP is supplied by the host.
use super::user_skills_import::SkillImportError;

pub const MAX_SKILL_ARCHIVE_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_API_ANSWER_BYTES: usize = 1024 * 1024;
pub const MAX_GITHUB_REF_SEGMENTS: usize = 4;

/// The host must validate every redirect, enforce the byte limit while reading,
/// classify HTTP errors, and propagate cancellation as a `cancelled` error.
pub type GitHubGet<'a> = dyn FnMut(&str, &str, usize) -> Result<Vec<u8>, SkillImportError> + 'a;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubSkillAddress {
    pub owner: String,
    pub repo: String,
    pub ref_: Option<String>,
    pub path: String,
    pub skill: Option<String>,
    pub ref_path: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGitHubSkillAddress {
    pub owner: String,
    pub repo: String,
    pub ref_: String,
    pub path: String,
    pub commit: String,
    pub skill: Option<String>,
}

fn owner_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 39
        && b[0].is_ascii_alphanumeric()
        && b.iter().enumerate().all(|(i, c)| {
            c.is_ascii_alphanumeric()
                || (*c == b'-' && i + 1 < b.len() && b[i + 1].is_ascii_alphanumeric())
        })
}
fn repo_name(s: &str) -> Option<&str> {
    let s = s.strip_suffix(".git").unwrap_or(s);
    if s.is_empty()
        || s.len() > 100
        || s == "."
        || s == ".."
        || !s
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        None
    } else {
        Some(s)
    }
}
fn skill_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}
fn control(s: &str) -> bool {
    s.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
}
fn decode_segment(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = |b: u8| (b as char).to_digit(16).filter(|_| b.is_ascii_hexdigit());
            decoded.push((hex(bytes[i + 1])? * 16 + hex(bytes[i + 2])?) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let value = String::from_utf8(decoded).ok()?;
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains(['/', '\\'])
        || control(&value)
    {
        None
    } else {
        Some(value)
    }
}
fn ref_allowed(s: &str) -> bool {
    !s.chars()
        .any(|c| c.is_whitespace() || "~^:?*[\\".contains(c))
        && !s.contains("@{")
        && !s.contains("..")
        && !s.ends_with(".lock")
        && !s.starts_with('.')
        && !s.ends_with('.')
}
fn address(owner: &str, repo: &str, skill: Option<String>) -> Option<GitHubSkillAddress> {
    if !owner_ok(owner) {
        return None;
    }
    Some(GitHubSkillAddress {
        owner: owner.into(),
        repo: repo_name(repo)?.into(),
        ref_: None,
        path: String::new(),
        skill,
        ref_path: None,
    })
}

pub fn parse_github_skill_address(input: &str) -> Option<GitHubSkillAddress> {
    let text = input.trim();
    if text.is_empty()
        || text.chars().any(char::is_whitespace)
        || control(text)
        || text.contains('\\')
    {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if lower == "https://github.com" || lower.starts_with("https://github.com/") {
        let rest = text[18..].split('#').next()?;
        if rest.contains('?') {
            return None;
        }
        let rest = rest
            .strip_prefix('/')
            .unwrap_or(rest)
            .strip_suffix('/')
            .unwrap_or(rest.strip_prefix('/').unwrap_or(rest));
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() < 2 {
            return None;
        }
        let mut result = address(parts[0], parts[1], None)?;
        if parts.len() == 2 {
            return Some(result);
        }
        let kind = parts[2];
        if kind != "tree" && kind != "blob" {
            return None;
        }
        let mut tail = &parts[3..];
        if kind == "blob" {
            if tail.len() < 2 || tail.last() != Some(&"SKILL.md") {
                return None;
            }
            tail = &tail[..tail.len() - 1];
        }
        if tail.is_empty() {
            return None;
        }
        let segments: Vec<String> = tail
            .iter()
            .map(|s| decode_segment(s))
            .collect::<Option<_>>()?;
        if !ref_allowed(&segments[0]) {
            return None;
        }
        result.ref_ = Some(segments[0].clone());
        result.path = segments[1..].join("/");
        result.ref_path = Some(segments);
        return Some(result);
    }
    if lower.starts_with("https://skills.sh/") {
        let rest = text[18..].split('#').next()?;
        if rest.contains('?') {
            return None;
        }
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 3 {
            return None;
        }
        let skill = decode_segment(parts[2])?;
        if !skill_ok(&skill) {
            return None;
        }
        return address(parts[0], parts[1], Some(skill));
    }
    let mut parts = text.split('/');
    let owner = parts.next()?;
    let repo_skill = parts.next()?;
    if parts.next().is_some() || owner.contains('@') {
        return None;
    }
    let mut split = repo_skill.split('@');
    let repo = split.next()?;
    let skill = split.next();
    if split.next().is_some() || skill.is_some_and(|s| !skill_ok(s)) {
        return None;
    }
    address(owner, repo, skill.map(str::to_owned))
}

pub fn validate_repo(repo: &str) -> Result<(), SkillImportError> {
    let valid = parse_github_skill_address(repo).is_some_and(|a| a.skill.is_none());
    if valid {
        Ok(())
    } else {
        Err(SkillImportError::new(
            "not-github",
            "The source record names no repository",
        ))
    }
}
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
fn api_repo(owner: &str, repo: &str) -> String {
    format!(
        "https://api.github.com/repos/{}/{}",
        encode(owner),
        encode(repo)
    )
}

pub fn github_default_branch(
    owner: &str,
    repo: &str,
    get: &mut GitHubGet<'_>,
) -> Result<String, SkillImportError> {
    let bytes = get(
        &api_repo(owner, repo),
        "application/vnd.github+json",
        MAX_API_ANSWER_BYTES,
    )?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    json.get("default_branch")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && !control(s))
        .map(str::to_owned)
        .ok_or_else(|| SkillImportError::new("network", "GitHub did not name a default branch"))
}
pub fn github_ref_commit(
    owner: &str,
    repo: &str,
    ref_: &str,
    get: &mut GitHubGet<'_>,
) -> Result<String, SkillImportError> {
    let ref_encoded = ref_.split('/').map(encode).collect::<Vec<_>>().join("/");
    let bytes = get(
        &format!("{}/commits/{}", api_repo(owner, repo), ref_encoded),
        "application/vnd.github.sha",
        MAX_API_ANSWER_BYTES,
    )?;
    let text = String::from_utf8_lossy(&bytes);
    let sha = text.trim();
    if !matches!(sha.len(), 40 | 64)
        || !sha
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(SkillImportError::new(
            "network",
            "GitHub did not answer with a commit",
        ));
    }
    Ok(sha.to_owned())
}
pub fn resolve_github_skill_address(
    address: &GitHubSkillAddress,
    get: &mut GitHubGet<'_>,
) -> Result<ResolvedGitHubSkillAddress, SkillImportError> {
    let result = |ref_: String, path: String, commit: String| ResolvedGitHubSkillAddress {
        owner: address.owner.clone(),
        repo: address.repo.clone(),
        ref_,
        path,
        commit,
        skill: address.skill.clone(),
    };
    if let Some(segments) = &address.ref_path {
        let tries = segments.len().min(MAX_GITHUB_REF_SEGMENTS);
        for count in 1..=tries {
            if !segments[..count].iter().all(|s| ref_allowed(s)) {
                break;
            }
            let ref_ = segments[..count].join("/");
            match github_ref_commit(&address.owner, &address.repo, &ref_, get) {
                Ok(commit) => return Ok(result(ref_, segments[count..].join("/"), commit)),
                Err(error) if error.kind == "not-found" && count < tries => continue,
                Err(error) => return Err(error),
            }
        }
        Err(SkillImportError::new("not-found", "No such branch"))
    } else {
        let ref_ = match &address.ref_ {
            Some(value) => value.clone(),
            None => github_default_branch(&address.owner, &address.repo, get)?,
        };
        let commit = github_ref_commit(&address.owner, &address.repo, &ref_, get)?;
        Ok(result(ref_, address.path.clone(), commit))
    }
}
pub fn download_github_archive(
    owner: &str,
    repo: &str,
    commit: &str,
    max_bytes: Option<usize>,
    get: &mut GitHubGet<'_>,
) -> Result<Vec<u8>, SkillImportError> {
    get(
        &format!(
            "https://codeload.github.com/{}/{}/zip/{}",
            encode(owner),
            encode(repo),
            encode(commit)
        ),
        "application/zip",
        max_bytes.unwrap_or(MAX_SKILL_ARCHIVE_BYTES),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_addresses_and_rejects_unsafe_segments() {
        let a = parse_github_skill_address("https://skills.sh/owner/repo/my-skill#top").unwrap();
        assert_eq!(a.skill.as_deref(), Some("my-skill"));
        assert!(parse_github_skill_address("https://github.com/o/r/tree/main/%2e%2e").is_none());
        assert!(parse_github_skill_address("https://github.com/o/r/tree/main/a%2Fb").is_none());
        assert!(parse_github_skill_address("https://github.com.evil/o/r").is_none());
    }
    #[test]
    fn resolves_slash_ref_and_pins_archive() {
        let a =
            parse_github_skill_address("https://github.com/o/r/tree/feature/new/skill").unwrap();
        let sha = "a".repeat(40);
        let mut requests = Vec::new();
        let mut get = |url: &str, _: &str, _: usize| {
            requests.push(url.to_owned());
            if url.ends_with("/commits/feature") {
                Err(SkillImportError::new("not-found", "missing"))
            } else if url.ends_with("/commits/feature/new") {
                Ok(sha.as_bytes().to_vec())
            } else {
                Ok(vec![1, 2])
            }
        };
        let resolved = resolve_github_skill_address(&a, &mut get).unwrap();
        assert_eq!(resolved.ref_, "feature/new");
        assert_eq!(resolved.path, "skill");
        assert_eq!(
            download_github_archive(
                &resolved.owner,
                &resolved.repo,
                &resolved.commit,
                None,
                &mut get
            )
            .unwrap(),
            vec![1, 2]
        );
        assert_eq!(
            requests.last().unwrap(),
            &format!("https://codeload.github.com/o/r/zip/{sha}")
        );
    }
    #[test]
    fn rate_limits_do_not_probe_other_refs() {
        let a = parse_github_skill_address("o/r").unwrap();
        let mut get =
            |_: &str, _: &str, _: usize| Err(SkillImportError::new("rate-limited", "limited"));
        assert_eq!(
            resolve_github_skill_address(&a, &mut get).unwrap_err().kind,
            "rate-limited"
        );
    }
}
