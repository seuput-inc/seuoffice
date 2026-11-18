//! Steps an engine leaves to the host that loads it, by name, with JSON in and
//! out: what parse5 gives, the draw.io readers, and the readers of the engines
//! not ported yet. In the wasm build a step goes through the callback slot 0 of
//! `seu_host.host_callback(slot, ptr, len)`, the import the loader
//! (`@seuoffice/engine-wasm`) supplies for every engine: the input is
//! `{"name": ..., "input": ...}`, the answer `{"ok": value}` or
//! `{"error": {"name": ..., "message": ...}}`, the error the host's step threw.
//! The host writes the answer into memory it got from `seu_alloc` and this
//! crate frees it. A call that registered no slot 0 is refused: [`HostError`]
//! with the name `HostUnavailable`.
//!
//! A native host may register a process-wide callback before making calls.

use serde_json::Value;

/// What a step of the host threw, or that no host can take the step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub name: String,
    pub message: String,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.message)
    }
}

impl std::error::Error for HostError {}

#[cfg(not(target_arch = "wasm32"))]
type NativeHost = dyn Fn(&str, &Value) -> Result<Value, HostError> + Send + Sync + 'static;

#[cfg(not(target_arch = "wasm32"))]
static NATIVE_HOST: std::sync::OnceLock<Box<NativeHost>> = std::sync::OnceLock::new();

/// Installs the native host once for all threads.
#[cfg(not(target_arch = "wasm32"))]
pub fn register_native_host(
    host: impl Fn(&str, &Value) -> Result<Value, HostError> + Send + Sync + 'static,
) -> Result<(), HostError> {
    NATIVE_HOST.set(Box::new(host)).map_err(|_| HostError {
        name: "HostAlreadyRegistered".to_owned(),
        message: "a native host is already registered".to_owned(),
    })
}

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    static ACTIVE_NAMES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(not(target_arch = "wasm32"))]
struct ActiveName;

#[cfg(not(target_arch = "wasm32"))]
impl Drop for ActiveName {
    fn drop(&mut self) {
        ACTIVE_NAMES.with(|active| { active.borrow_mut().pop(); });
    }
}

/// Runs the host's step `name` on `input`.
pub fn call(name: &str, input: &Value) -> Result<Value, HostError> {
    let answer = raw_call(name, &input.to_string())?;
    let mut answer: Value = serde_json::from_str(&answer).map_err(|error| HostError {
        name: "HostProtocolError".to_owned(),
        message: format!("the answer of {name} is not JSON: {error}"),
    })?;
    if let Some(value) = answer.get_mut("ok") {
        return Ok(value.take());
    }
    let error = answer.get("error");
    let field = |key: &str| {
        error
            .and_then(|error| error.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Err(HostError {
        name: field("name"),
        message: field("message"),
    })
}

/// The callback slot the loader reserves for host steps.
#[cfg(target_arch = "wasm32")]
const HOST_STEP_SLOT: u32 = 0;

#[cfg(target_arch = "wasm32")]
fn raw_call(name: &str, input: &str) -> Result<String, HostError> {
    #[link(wasm_import_module = "seu_host")]
    unsafe extern "C" {
        /// The callback registered under `slot` for the running call gets the
        /// UTF-8 text at `ptr`, `len` and answers text, written into memory
        /// the host got from `seu_alloc`, packed `(ptr << 32) | len`.
        /// `u64::MAX`: no such callback, or it threw.
        fn host_callback(slot: u32, ptr: *const u8, len: u32) -> u64;
    }
    let request = format!(
        "{{\"name\":{},\"input\":{input}}}",
        serde_json::Value::String(name.to_owned())
    );
    // SAFETY: the host only reads `request` during the call; a length fits u32 on wasm32
    let packed = unsafe { host_callback(HOST_STEP_SLOT, request.as_ptr(), request.len() as u32) };
    if packed == u64::MAX {
        return Err(HostError {
            name: "HostUnavailable".to_owned(),
            message: format!("{name} is a step of the host, and this call has no host steps"),
        });
    }
    let (pointer, length) = ((packed >> 32) as usize, packed as u32 as usize);
    // SAFETY: the host wrote `length` bytes at `pointer`, a buffer `seu_alloc`
    // made as a boxed slice of exactly that length; it is freed here, once
    let answer = unsafe {
        Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            pointer as *mut u8,
            length,
        ))
    };
    String::from_utf8(answer.into_vec()).map_err(|error| HostError {
        name: "HostProtocolError".to_owned(),
        message: format!("the answer of {name} is not UTF-8: {error}"),
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn raw_call(name: &str, input: &str) -> Result<String, HostError> {
    let host = NATIVE_HOST.get().ok_or_else(|| HostError {
        name: "HostUnavailable".to_owned(),
        message: format!("{name} is a step of the host, and a native build has none"),
    })?;
    if name.is_empty() {
        return Err(HostError { name: "HostProtocolError".into(), message: "empty host step name".into() });
    }
    let recursive = ACTIVE_NAMES.with(|active| {
        let mut active = active.borrow_mut();
        if active.iter().any(|item| item == name) {
            true
        } else {
            active.push(name.to_owned());
            false
        }
    });
    if recursive {
        return Err(HostError { name: "HostRecursion".into(), message: format!("recursive host step: {name}") });
    }
    let _active = ActiveName;
    let input = serde_json::from_str(input).map_err(|error| HostError {
        name: "HostProtocolError".into(), message: format!("invalid host input: {error}"),
    })?;
    let value = host(name, &input)?;
    Ok(serde_json::json!({ "ok": value }).to_string())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn native_callback_is_shared_and_rejects_duplicates_and_recursion() {
        assert_eq!(call("test", &Value::Null).unwrap_err().name, "HostUnavailable");
        register_native_host(|name, value| match name {
            "echo" => Ok(value.clone()),
            "recurse" => call("recurse", value),
            _ => Err(HostError { name: "UnknownHostStep".into(), message: name.into() }),
        }).unwrap();
        assert_eq!(register_native_host(|_, _| Ok(Value::Null)).unwrap_err().name, "HostAlreadyRegistered");
        assert_eq!(std::thread::spawn(|| call("echo", &serde_json::json!(42))).join().unwrap().unwrap(), serde_json::json!(42));
        assert_eq!(call("recurse", &Value::Null).unwrap_err().name, "HostRecursion");
        assert_eq!(call("unknown", &Value::Null).unwrap_err().name, "UnknownHostStep");
        assert_eq!(call("", &Value::Null).unwrap_err().name, "HostProtocolError");
    }
}
