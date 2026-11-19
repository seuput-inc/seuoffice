//! Private NSS helper protocol and owned-process cleanup.
use super::nss_bundle::NssChild;
use std::io::{Read, Write};
use zeroize::Zeroizing;
fn exact(input: &mut impl Read, bytes: &mut [u8]) -> Result<(), String> {
    input
        .read_exact(bytes)
        .map_err(|_| "NSS helper protocol failure".into())
}
fn field(input: &mut impl Read, length: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut result = Zeroizing::new(Vec::new());
    let mut chunk = Zeroizing::new([0u8; 32768]);
    let mut left = length;
    while left != 0 {
        let count = left.min(chunk.len());
        exact(input, &mut chunk[..count])?;
        result
            .len()
            .checked_add(count)
            .ok_or("signing frame length overflow")?;
        result
            .try_reserve(count)
            .map_err(|_| "signing frame allocation failed")?;
        result.extend_from_slice(&chunk[..count]);
        left -= count;
    }
    Ok(result)
}
fn length(bytes: &[u8]) -> Result<usize, String> {
    let value = u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| "NSS helper protocol failure")?,
    );
    usize::try_from(value).map_err(|_| "signing frame length overflow".into())
}
#[derive(Debug)]
enum Response {
    Identity(Zeroizing<Vec<u8>>, Zeroizing<String>),
    StoreFailure(&'static str),
}

fn read_response(
    input: &mut impl Read,
    output: &mut impl Write,
    mut confirm: impl FnMut(&str) -> Result<bool, String>,
) -> Result<Response, String> {
    loop {
        let mut header = [0u8; 17];
        exact(input, &mut header)?;
        let first = length(&header[1..9])?;
        let second = length(&header[9..17])?;
        match header[0] {
            5 if second == 0 && first <= 4096 => {
                let name = field(input, first)?;
                let name = std::str::from_utf8(&name).map_err(|_| "NSS helper protocol failure")?;
                let accepted = confirm(name)?;
                output
                    .write_all(&[2, u8::from(accepted)])
                    .and_then(|_| output.flush())
                    .map_err(|_| "NSS helper protocol failure")?;
            }
            0 if first != 0 && second != 0 => {
                let p12 = field(input, first)?;
                let password = field(input, second)?;
                let password = Zeroizing::new(
                    std::str::from_utf8(&password)
                        .map_err(|_| "NSS helper protocol failure")?
                        .to_owned(),
                );
                let mut end = [0u8; 1];
                if input
                    .read(&mut end)
                    .map_err(|_| "NSS helper protocol failure")?
                    != 0
                {
                    return Err("NSS helper protocol failure".into());
                }
                return Ok(Response::Identity(p12, password));
            }
            status @ 1..=3 if first == 0 && second == 0 => {
                let mut end = [0u8; 1];
                if input
                    .read(&mut end)
                    .map_err(|_| "NSS helper protocol failure")?
                    != 0
                {
                    return Err("NSS helper protocol failure".into());
                }
                return Ok(Response::StoreFailure(match status {
                    1 => "cert-store-locked",
                    2 => "cert-not-exportable",
                    _ => "cert-store-tool-missing",
                }));
            }
            _ => return Err("NSS helper protocol failure".into()),
        }
    }
}
pub(super) fn exchange(
    mut child: NssChild,
    dir: &str,
    nickname: &str,
    password: &str,
    confirm: impl FnMut(&str) -> Result<bool, String>,
) -> Result<(Zeroizing<Vec<u8>>, Zeroizing<String>), String> {
    let result = (|| {
        let mut input = child.take_stdin().ok_or("NSS helper protocol failure")?;
        let mut output = child.take_stdout().ok_or("NSS helper protocol failure")?;
        let fields = [dir.as_bytes(), nickname.as_bytes(), password.as_bytes()];
        let mut header = [0u8; 26];
        header[0] = 2;
        header[1] = 1;
        for (index, field) in fields.iter().enumerate() {
            if field.contains(&0) || field.len() > u32::MAX as usize {
                return Err("NSS helper protocol failure".into());
            }
            header[2 + 8 * index..10 + 8 * index].copy_from_slice(
                &u64::try_from(field.len())
                    .map_err(|_| "signing frame length overflow")?
                    .to_be_bytes(),
            );
        }
        input
            .write_all(&header)
            .map_err(|_| "NSS helper protocol failure")?;
        for field in fields {
            input
                .write_all(field)
                .map_err(|_| "NSS helper protocol failure")?;
        }
        input.flush().map_err(|_| "NSS helper protocol failure")?;
        read_response(&mut output, &mut input, confirm)
    })();
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            if child.terminate_and_reap().is_err() {
                return Err(format!("{error}; NSS helper cleanup failed"));
            }
            return Err(error);
        }
    };
    let success = child.wait().map_err(|_| "NSS helper cleanup failed")?;
    match (response, success) {
        (Response::Identity(p12, password), true) => Ok((p12, password)),
        (Response::StoreFailure(error), false) => Err(error.into()),
        _ => Err("NSS helper protocol failure".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn header(status: u8, first: u64, second: u64) -> Vec<u8> {
        let mut bytes = vec![status];
        bytes.extend(first.to_be_bytes());
        bytes.extend(second.to_be_bytes());
        bytes
    }
    #[test]
    fn confirmation_is_bound_to_response_and_rejects_trailing_data() {
        let mut input = header(5, 5, 0);
        input.extend(b"token");
        input.extend(header(0, 3, 2));
        input.extend(b"p12pw");
        let mut replies = Vec::new();
        let Response::Identity(p12, password) =
            read_response(&mut input.as_slice(), &mut replies, |name| {
                Ok(name == "token")
            })
            .unwrap()
        else {
            panic!("expected identity")
        };
        assert_eq!(&*p12, b"p12");
        assert_eq!(&*password, "pw");
        assert_eq!(replies, [2, 1]);
        input.push(7);
        assert!(read_response(&mut input.as_slice(), &mut Vec::new(), |_| Ok(false)).is_err());
        assert!(read_response(&mut &input[..20], &mut Vec::new(), |_| Ok(false)).is_err());
    }
    #[test]
    fn denied_or_repeated_prompts_keep_closed_statuses() {
        let mut input = header(5, 1, 0);
        input.extend(b"a");
        input.extend(header(5, 1, 0));
        input.extend(b"b");
        input.extend(header(1, 0, 0));
        let mut replies = Vec::new();
        assert!(matches!(
            read_response(&mut input.as_slice(), &mut replies, |name| Ok(name == "a")),
            Ok(Response::StoreFailure("cert-store-locked"))
        ));
        assert_eq!(replies, [2, 1, 2, 0]);
        assert!(
            read_response(&mut header(5, 1, 1).as_slice(), &mut Vec::new(), |_| Ok(
                true
            ))
            .is_err()
        );
    }
    #[test]
    fn malformed_response_kills_and_reaps_owned_child() {
        use std::process::{Command, Stdio};
        let child = Command::new("/bin/sh")
            .args(["-c", "printf X0000000000000000; exec sleep 60"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let child = NssChild::from_test_child(child);
        assert_eq!(
            exchange(child, "synthetic", "synthetic", "", |_| Ok(false)).unwrap_err(),
            "NSS helper protocol failure"
        );
        // SAFETY: pid was the test-owned child and exchange must already reap it.
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
