//! Mandatory Linux sandbox used for command Adapter processes.
//!
//! The coordinator starts the tiny `reginux-sandbox` launcher with an empty
//! environment and sends this request over stdin.  The launcher rechecks the
//! executable digest, installs resource limits, Landlock and seccomp, and only
//! then replaces itself with the declared executable.

use std::collections::BTreeMap;
use std::convert::TryInto;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, PathBeneath, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::filesystem::open_regular_file;
use crate::model::NetworkAccess;

pub const SANDBOX_REQUEST_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxRequest {
    pub program: PathBuf,
    pub expected_digest: String,
    pub args: Vec<String>,
    pub read_paths: Vec<PathBuf>,
    pub network: NetworkAccess,
}

pub fn read_request() -> Result<SandboxRequest> {
    let mut bytes = Vec::new();
    io::stdin()
        .take(SANDBOX_REQUEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .context("read sandbox request")?;
    if bytes.len() as u64 > SANDBOX_REQUEST_LIMIT {
        bail!("sandbox request exceeds the 1 MiB limit");
    }
    serde_json::from_slice(&bytes).context("decode sandbox request")
}

/// Apply the mandatory policy and exec the requested program.  This function
/// returns only when validation or exec fails.
pub fn exec_request(request: SandboxRequest) -> Result<()> {
    let executable = validate_request(&request)?;
    apply_resource_limits()?;
    close_inherited_file_descriptors(executable.as_raw_fd())?;
    apply_landlock(&request, &executable)?;
    apply_seccomp(&request.network)?;
    exec_verified(&executable, &request)
}

fn validate_request(request: &SandboxRequest) -> Result<File> {
    let mut executable = open_regular_file(&request.program)?;
    let metadata = executable
        .metadata()
        .with_context(|| format!("inspect sandbox target {}", request.program.display()))?;
    if metadata.len() > 64 * 1024 * 1024 {
        bail!("sandbox target exceeds the 64 MiB executable limit");
    }
    let mut bytes = Vec::new();
    executable
        .by_ref()
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read sandbox target {}", request.program.display()))?;
    if bytes.len() > 64 * 1024 * 1024 {
        bail!("sandbox target exceeds the 64 MiB executable limit");
    }
    let digest = format!("sha256:{:x}", Sha256::digest(bytes));
    if digest != request.expected_digest {
        bail!("sandbox target digest changed after approval");
    }
    if request.args.len() > 256
        || request
            .args
            .iter()
            .any(|argument| argument.len() > 64 * 1024 || argument.contains('\0'))
    {
        bail!("sandbox argument limits exceeded");
    }
    for path in &request.read_paths {
        validate_absolute_no_symlink(path)?;
        if !path.exists() {
            bail!(
                "declared sandbox read path does not exist: {}",
                path.display()
            );
        }
    }
    Ok(executable)
}

fn validate_absolute_no_symlink(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("sandbox paths must be absolute: {}", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("sandbox path traversal is forbidden: {}", path.display());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("inspect sandbox path {}", current.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "sandbox path contains a symbolic link: {}",
                current.display()
            );
        }
    }
    Ok(())
}

fn apply_resource_limits() -> Result<()> {
    set_limit(libc::RLIMIT_CORE, 0, 0)?;
    set_limit(libc::RLIMIT_NOFILE, 64, 64)?;
    set_limit(libc::RLIMIT_NPROC, 32, 32)?;
    set_limit(libc::RLIMIT_AS, 512 * 1024 * 1024, 512 * 1024 * 1024)?;
    set_limit(libc::RLIMIT_CPU, 30, 30)?;
    set_limit(libc::RLIMIT_FSIZE, 2 * 1024 * 1024, 2 * 1024 * 1024)?;
    Ok(())
}

fn close_inherited_file_descriptors(keep_fd: std::os::fd::RawFd) -> Result<()> {
    if keep_fd < 3 {
        bail!("verified sandbox executable has an unsafe descriptor");
    }
    let keep_fd = keep_fd as libc::c_ulong;
    let close_range = |start: libc::c_ulong, end: libc::c_ulong| -> Result<bool> {
        if start > end {
            return Ok(true);
        }
        let result =
            unsafe { libc::syscall(libc::SYS_close_range, start, end, 0 as libc::c_ulong) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOSYS) {
            return Ok(false);
        }
        Err(error).context("close inherited sandbox file descriptors")
    };

    if close_range(3, keep_fd - 1)? && close_range(keep_fd + 1, u32::MAX as libc::c_ulong)? {
        return Ok(());
    }

    let descriptors = fs::read_dir("/proc/self/fd")
        .context("enumerate inherited sandbox file descriptors")?
        .filter_map(|entry| {
            entry
                .ok()?
                .file_name()
                .to_str()?
                .parse::<libc::c_int>()
                .ok()
        })
        .filter(|fd| *fd > 2 && *fd as libc::c_ulong != keep_fd)
        .collect::<Vec<_>>();
    for fd in descriptors {
        unsafe {
            libc::close(fd);
        }
    }
    Ok(())
}

fn set_limit(resource: libc::__rlimit_resource_t, soft: u64, hard: u64) -> Result<()> {
    let limit = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    if unsafe { libc::setrlimit(resource, &limit) } == -1 {
        return Err(io::Error::last_os_error()).context("set sandbox resource limit");
    }
    Ok(())
}

fn apply_landlock(request: &SandboxRequest, executable: &File) -> Result<()> {
    // ABI V3 adds truncate and refer mediation.  Running command Adapters on a
    // kernel older than 6.2 is refused instead of silently dropping isolation.
    let abi = ABI::V3;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);
    let mut paths = vec![
        PathBuf::from("/usr"),
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
        PathBuf::from("/etc/ld.so.cache"),
        PathBuf::from("/etc/localtime"),
        PathBuf::from("/dev/null"),
        PathBuf::from("/dev/urandom"),
        // fexecve of a shebang script exposes the already-verified descriptor
        // to its interpreter through this process-local directory.
        PathBuf::from("/proc/self/fd"),
    ];
    paths.extend(request.read_paths.iter().cloned());
    paths.retain(|path| path.exists());
    paths.sort();
    paths.dedup();

    let status = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(access_all)?
        .create()?
        .add_rules(path_beneath_rules(paths, access_read))?
        .add_rule(PathBeneath::new(
            executable
                .try_clone()
                .context("clone executable descriptor")?,
            AccessFs::Execute | AccessFs::ReadFile,
        ))?
        .restrict_self()?;
    if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
        bail!("Landlock policy was not fully enforced");
    }
    Ok(())
}

fn exec_verified(executable: &File, request: &SandboxRequest) -> Result<()> {
    let mut arguments = Vec::with_capacity(request.args.len() + 1);
    arguments.push(
        CString::new(request.program.as_os_str().as_bytes())
            .context("sandbox program path contains NUL")?,
    );
    for argument in &request.args {
        arguments.push(CString::new(argument.as_bytes()).context("sandbox argument contains NUL")?);
    }
    let environment = [
        CString::new("PATH=/usr/bin:/bin")?,
        CString::new("LANG=C.UTF-8")?,
        CString::new("LC_ALL=C.UTF-8")?,
    ];
    let mut argument_pointers = arguments
        .iter()
        .map(|argument| argument.as_ptr())
        .collect::<Vec<_>>();
    argument_pointers.push(std::ptr::null());
    let mut environment_pointers = environment
        .iter()
        .map(|variable| variable.as_ptr())
        .collect::<Vec<_>>();
    environment_pointers.push(std::ptr::null());

    // Linux requires the descriptor to survive exec when the verified target
    // is a shebang script so the interpreter can reopen that exact file.
    let descriptor_flags = unsafe { libc::fcntl(executable.as_raw_fd(), libc::F_GETFD) };
    if descriptor_flags == -1
        || unsafe {
            libc::fcntl(
                executable.as_raw_fd(),
                libc::F_SETFD,
                descriptor_flags & !libc::FD_CLOEXEC,
            )
        } == -1
    {
        return Err(io::Error::last_os_error()).context("prepare verified executable descriptor");
    }
    unsafe {
        libc::fexecve(
            executable.as_raw_fd(),
            argument_pointers.as_ptr(),
            environment_pointers.as_ptr(),
        );
    }
    Err(io::Error::last_os_error())
        .with_context(|| format!("exec verified sandbox target {}", request.program.display()))
}

fn unconditional_rule() -> Vec<SeccompRule> {
    Vec::new()
}

fn apply_seccomp(network: &NetworkAccess) -> Result<()> {
    let mut rules = BTreeMap::new();
    for syscall in dangerous_syscalls() {
        rules.insert(syscall, unconditional_rule());
    }
    // Permit same-process threads, which many CLI runtimes need, while still
    // denying clone calls that could create a child process. clone3 remains
    // denied because seccomp cannot safely inspect its pointed-to flags.
    rules.insert(
        libc::SYS_clone,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::MaskedEq(libc::CLONE_THREAD as u64),
            0,
        )?])?],
    );

    match network {
        NetworkAccess::Internet => {}
        NetworkAccess::None => {
            for syscall in network_syscalls() {
                rules.insert(syscall, unconditional_rule());
            }
        }
        NetworkAccess::Local => {
            rules.insert(
                libc::SYS_socket,
                vec![SeccompRule::new(vec![SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_UNIX as u64,
                )?])?],
            );
        }
    }

    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH
            .try_into()
            .map_err(|_| anyhow!("unsupported architecture for seccomp"))?,
    )?
    .try_into()?;
    seccompiler::apply_filter(&filter).context("install seccomp filter")?;
    Ok(())
}

fn dangerous_syscalls() -> Vec<i64> {
    vec![
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_ptrace,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_reboot,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_clone3,
        libc::SYS_fork,
        libc::SYS_vfork,
    ]
}

fn network_syscalls() -> Vec<i64> {
    vec![
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_rejects_relative_programs_and_paths() {
        let request = SandboxRequest {
            program: PathBuf::from("bin/true"),
            expected_digest: String::new(),
            args: Vec::new(),
            read_paths: Vec::new(),
            network: NetworkAccess::None,
        };
        assert!(validate_request(&request).is_err());
        assert!(validate_absolute_no_symlink(Path::new("relative")).is_err());
    }

    #[test]
    fn network_policy_covers_datagram_batch_syscalls() {
        let syscalls = network_syscalls();
        assert!(syscalls.contains(&libc::SYS_sendmmsg));
        assert!(syscalls.contains(&libc::SYS_recvmmsg));
    }
}
