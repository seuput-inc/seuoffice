use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

const TAIL_BYTES: u64 = 8 * 1024 * 1024 - 1;
const HARD_CAP: u64 = 64 * 1024 * 1024;

fn records(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| {
            value
                .get("seq")
                .and_then(Value::as_f64)
                .is_some_and(f64::is_finite)
                && value.get("role").and_then(Value::as_str).is_some()
                && value.get("text").and_then(Value::as_str).is_some()
        })
        .collect()
}

pub(super) fn read_tail(path: &Path) -> io::Result<Vec<Value>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut length = size.min(TAIL_BYTES);
    loop {
        let start = size - length;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; length as usize];
        file.read_exact(&mut bytes)?;
        if start == 0 {
            return Ok(records(&bytes));
        }
        file.seek(SeekFrom::Start(start - 1))?;
        let mut boundary = [0];
        file.read_exact(&mut boundary)?;
        if boundary[0] == b'\n' {
            return Ok(records(&bytes));
        }
        if let Some(index) = bytes.iter().position(|byte| *byte == b'\n') {
            if index + 1 < bytes.len() {
                return Ok(records(&bytes[index + 1..]));
            }
        }
        if length >= HARD_CAP {
            return Ok(Vec::new());
        }
        length = size.min(length * 2).min(HARD_CAP);
    }
}

pub(super) fn append(path: &Path, messages: &[Value]) -> io::Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| io::Error::other("chat path has no parent"))?,
    )?;
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    let size = file.metadata()?.len();
    if size > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    for message in messages {
        serde_json::to_writer(&mut file, message)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

fn temporary_path(path: &Path) -> io::Result<PathBuf> {
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{suffix}.tmp"));
    Ok(PathBuf::from(name))
}

pub(super) fn merge(source: &Path, target: &Path) -> io::Result<Option<f64>> {
    let source_records = records(&fs::read(source)?);
    let target_bytes = fs::read(target)?;
    let mut next = records(&target_bytes)
        .iter()
        .filter_map(|record| record.get("seq").and_then(Value::as_f64))
        .fold(-1.0_f64, f64::max)
        + 1.0;
    if source_records.is_empty() {
        fs::remove_file(source)?;
        return Ok(Some(next - 1.0));
    }
    let temp = temporary_path(target)?;
    let result = (|| {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        out.write_all(&target_bytes)?;
        if !target_bytes.is_empty() && !target_bytes.ends_with(b"\n") {
            out.write_all(b"\n")?;
        }
        for mut record in source_records {
            record["seq"] = serde_json::Number::from_f64(next)
                .map(Value::Number)
                .ok_or_else(|| io::Error::other("chat sequence overflow"))?;
            next += 1.0;
            serde_json::to_writer(&mut out, &record)?;
            out.write_all(b"\n")?;
        }
        drop(out);
        fs::rename(&temp, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    fs::remove_file(source)?;
    Ok(Some(next - 1.0))
}
