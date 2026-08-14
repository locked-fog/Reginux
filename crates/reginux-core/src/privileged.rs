//! Narrow privileged protocol for system configuration writes.
//!
//! The unprivileged coordinator can only request compare-and-replace edits.
//! The helper independently authorizes every path, verifies the original
//! bytes, prepares backups, and rolls the whole set back on failure.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::filesystem::{
    atomic_write, atomic_write_checked, file_mode, read_regular_file_limited,
    remove_regular_file_checked,
};

pub const HELPER_PROTOCOL_VERSION: u32 = 1;
pub const HELPER_MESSAGE_LIMIT: u64 = 8 * 1024 * 1024;
pub const HELPER_FILE_LIMIT: usize = 1024 * 1024;
pub const HELPER_FILE_COUNT_LIMIT: usize = 64;
const HELPER_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SystemAuthorization {
    Builtin,
    Plugin {
        plugin_id: String,
        manifest_path: PathBuf,
        plugin_digest: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivilegedFileEdit {
    pub path: PathBuf,
    pub expected_original_base64: String,
    pub replacement_base64: String,
    pub mode: Option<u32>,
    pub delete: bool,
    pub authorization: SystemAuthorization,
}

impl PrivilegedFileEdit {
    pub fn new(
        path: PathBuf,
        expected_original: &[u8],
        replacement: &[u8],
        mode: Option<u32>,
        authorization: SystemAuthorization,
        delete: bool,
    ) -> Self {
        Self {
            path,
            expected_original_base64: BASE64.encode(expected_original),
            replacement_base64: BASE64.encode(replacement),
            mode,
            delete,
            authorization,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivilegedRequest {
    pub protocol: u32,
    pub transaction_id: String,
    pub backup: bool,
    pub files: Vec<PrivilegedFileEdit>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivilegedResponse {
    pub ok: bool,
    pub message: String,
    pub backups: Vec<PathBuf>,
}

struct CheckedEdit {
    path: PathBuf,
    original: Vec<u8>,
    replacement: Vec<u8>,
    mode: Option<u32>,
    existed: bool,
    delete: bool,
}

pub fn handle_privileged_request(request: PrivilegedRequest) -> Result<PrivilegedResponse> {
    if request.protocol != HELPER_PROTOCOL_VERSION {
        bail!("unsupported helper protocol {}", request.protocol);
    }
    validate_transaction_id(&request.transaction_id)?;
    if request.files.is_empty() || request.files.len() > HELPER_FILE_COUNT_LIMIT {
        bail!("helper request must contain 1..={HELPER_FILE_COUNT_LIMIT} files");
    }

    let mut checked = Vec::with_capacity(request.files.len());
    let mut unique_paths = HashSet::with_capacity(request.files.len());
    for edit in request.files {
        if !unique_paths.insert(edit.path.clone()) {
            bail!("duplicate helper target {}", edit.path.display());
        }
        let expected = decode_file_bytes(&edit.expected_original_base64, "expected original")?;
        let replacement = decode_file_bytes(&edit.replacement_base64, "replacement")?;
        authorize_system_path(&edit.path, &edit.authorization)?;
        let (current, existed) = match fs::symlink_metadata(&edit.path) {
            Ok(_) => (
                read_regular_file_limited(&edit.path, HELPER_FILE_LIMIT as u64)?,
                true,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", edit.path.display()))
            }
        };
        if current != expected {
            bail!(
                "{} changed since staging; privileged transaction refused",
                edit.path.display()
            );
        }
        checked.push(CheckedEdit {
            path: edit.path,
            original: current,
            replacement,
            mode: edit.mode,
            existed,
            delete: edit.delete,
        });
    }

    let mut backups = Vec::new();
    if request.backup {
        for edit in &checked {
            if edit.existed {
                backups.push(write_system_backup(
                    &request.transaction_id,
                    &edit.path,
                    &edit.original,
                )?);
            }
        }
    }

    let mut applied = Vec::new();
    for edit in &checked {
        let write_result = verify_checked_current(edit).and_then(|()| {
            if edit.delete {
                remove_regular_file_checked(&edit.path, &edit.original)
                    .with_context(|| format!("remove system file {}", edit.path.display()))
            } else {
                atomic_write_checked(
                    &edit.path,
                    edit.existed.then_some(edit.original.as_slice()),
                    &edit.replacement,
                    edit.mode,
                )
            }
        });
        if let Err(error) = write_result {
            let rollback_errors = rollback_checked(&applied);
            if rollback_errors.is_empty() {
                bail!(
                    "failed to write {}; restored {} system file(s): {error}",
                    edit.path.display(),
                    applied.len()
                );
            }
            bail!(
                "failed to write {}; system rollback incomplete: {error}; {}",
                edit.path.display(),
                rollback_errors.join("; ")
            );
        }
        applied.push(edit);
    }

    Ok(PrivilegedResponse {
        ok: true,
        message: format!("applied {} system file(s)", checked.len()),
        backups,
    })
}

fn verify_checked_current(edit: &CheckedEdit) -> Result<()> {
    match fs::symlink_metadata(&edit.path) {
        Ok(_) if edit.existed => {
            if read_regular_file_limited(&edit.path, HELPER_FILE_LIMIT as u64)? != edit.original {
                bail!("{} changed during privileged apply", edit.path.display());
            }
        }
        Ok(_) => bail!(
            "{} was created during privileged apply",
            edit.path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !edit.existed => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "{} was removed during privileged apply",
                edit.path.display()
            )
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", edit.path.display()))
        }
    }
    Ok(())
}

pub fn invoke_privileged_helper(request: &PrivilegedRequest) -> Result<PrivilegedResponse> {
    let encoded = serde_json::to_vec(request).context("encode privileged request")?;
    if encoded.len() as u64 > HELPER_MESSAGE_LIMIT {
        bail!("privileged request exceeds the 8 MiB protocol limit");
    }
    let helper = helper_path()?;
    let mut command = if unsafe { libc::geteuid() } == 0 {
        Command::new(&helper)
    } else {
        let pkexec = Path::new("/usr/bin/pkexec");
        if !pkexec.is_file() {
            bail!("system changes require /usr/bin/pkexec and the Reginux polkit policy");
        }
        let mut command = Command::new(pkexec);
        command.arg(&helper);
        command
    };
    let mut child = command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start privileged helper {}", helper.display()))?;
    let mut helper_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("helper stdin is unavailable"))?;
    let helper_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("helper stdout is unavailable"))?;
    let helper_stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("helper stderr is unavailable"))?;
    let stdout_thread = thread::spawn(move || read_helper_stream(helper_stdout));
    let stderr_thread = thread::spawn(move || read_helper_stream(helper_stderr));

    if let Err(error) = helper_stdin.write_all(&encoded) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
        return Err(error).context("send privileged request");
    }
    drop(helper_stdin);

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("poll privileged helper")? {
            break status;
        }
        if started.elapsed() >= HELPER_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            bail!("privileged helper timed out after 120 seconds");
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = join_helper_stream(stdout_thread, "stdout")?;
    let stderr = join_helper_stream(stderr_thread, "stderr")?;
    if stdout.len() as u64 > HELPER_MESSAGE_LIMIT || stderr.len() as u64 > HELPER_MESSAGE_LIMIT {
        bail!("privileged helper response exceeded the protocol limit");
    }
    if !status.success() {
        bail!(
            "privileged helper failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    let response: PrivilegedResponse =
        serde_json::from_slice(&stdout).context("decode privileged helper response")?;
    if !response.ok {
        bail!(
            "privileged helper refused the request: {}",
            response.message
        );
    }
    Ok(response)
}

fn read_helper_stream(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .by_ref()
        .take(HELPER_MESSAGE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_helper_stream(
    handle: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    label: &str,
) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| anyhow!("privileged helper {label} reader panicked"))?
        .with_context(|| format!("read privileged helper {label}"))
}

fn helper_path() -> Result<PathBuf> {
    let installed = PathBuf::from("/usr/libexec/reginux-helper");
    if installed.is_file() {
        return Ok(installed);
    }
    let current = std::env::current_exe().context("resolve current executable")?;
    let sibling = current.with_file_name("reginux-helper");
    if sibling.is_file() {
        return Ok(sibling);
    }
    bail!("reginux-helper is not installed at /usr/libexec/reginux-helper")
}

fn decode_file_bytes(encoded: &str, label: &str) -> Result<Vec<u8>> {
    let decoded = BASE64
        .decode(encoded)
        .with_context(|| format!("decode {label}"))?;
    if decoded.len() > HELPER_FILE_LIMIT {
        bail!("{label} exceeds the 1 MiB helper limit");
    }
    Ok(decoded)
}

fn authorize_system_path(path: &Path, authorization: &SystemAuthorization) -> Result<()> {
    validate_absolute_path(path)?;
    match authorization {
        SystemAuthorization::Builtin => validate_builtin_path(path),
        SystemAuthorization::Plugin {
            plugin_id,
            manifest_path,
            plugin_digest,
        } => crate::plugin::authorize_system_schema_target(
            manifest_path,
            plugin_id,
            plugin_digest,
            path,
        ),
    }
}

fn validate_absolute_path(path: &Path) -> Result<()> {
    use std::path::Component;
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("helper paths must be absolute and contain no parent traversal");
    }
    if !crate::filesystem::is_system_path(path) {
        bail!("helper path is outside the system configuration roots");
    }
    Ok(())
}

fn validate_builtin_path(path: &Path) -> Result<()> {
    let allowed = matches!(
        path.to_str(),
        Some(
            "/etc/hostname"
                | "/etc/locale.conf"
                | "/etc/environment"
                | "/etc/hosts"
                | "/etc/sysctl.conf"
        )
    ) || (path.parent() == Some(Path::new("/etc/sysctl.d"))
        && path.extension().and_then(|extension| extension.to_str()) == Some("conf")
        && path.file_name().is_some());
    if !allowed {
        bail!("path is outside the built-in system write allowlist");
    }
    Ok(())
}

fn validate_transaction_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid helper transaction id");
    }
    Ok(())
}

fn write_system_backup(transaction_id: &str, path: &Path, contents: &[u8]) -> Result<PathBuf> {
    let relative = path
        .strip_prefix("/")
        .map_err(|_| anyhow!("system backup path is not absolute"))?;
    let destination = Path::new("/var/lib/reginux/backups")
        .join(transaction_id)
        .join(relative);
    atomic_write(&destination, contents, Some(0o600))?;
    Ok(destination)
}

fn rollback_checked(applied: &[&CheckedEdit]) -> Vec<String> {
    let mut errors = Vec::new();
    for edit in applied.iter().rev() {
        let result = if edit.delete {
            atomic_write_checked(&edit.path, None, &edit.original, edit.mode)
        } else if edit.existed {
            atomic_write_checked(
                &edit.path,
                Some(&edit.replacement),
                &edit.original,
                file_mode(&edit.path).or(edit.mode),
            )
        } else {
            remove_regular_file_checked(&edit.path, &edit.replacement)
                .with_context(|| format!("remove newly created {}", edit.path.display()))
        };
        if let Err(error) = result {
            errors.push(format!("{}: {error}", edit.path.display()));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_allowlist_is_narrow() {
        assert!(validate_builtin_path(Path::new("/etc/hostname")).is_ok());
        assert!(validate_builtin_path(Path::new("/etc/sysctl.d/99-reginux.conf")).is_ok());
        assert!(validate_builtin_path(Path::new("/etc/shadow")).is_err());
        assert!(validate_builtin_path(Path::new("/etc/sysctl.d/sub/value.conf")).is_err());
    }

    #[test]
    fn transaction_ids_cannot_escape_the_backup_root() {
        assert!(validate_transaction_id("20260812T120000.123").is_ok());
        assert!(validate_transaction_id("../../tmp").is_err());
        assert!(validate_transaction_id("").is_err());
    }
}
