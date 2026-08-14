use std::collections::hash_map::DefaultHasher;
use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::EditorConfig;

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("reginux")
}

pub fn state_dir() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("state"))
        .join("reginux")
}

pub fn expand_path(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(value)
}

pub fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("read {}", path.display()))
}

pub fn read_bytes_or_empty(path: &Path) -> Result<Vec<u8>> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_regular_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

/// Read an existing regular file without following a final-component symlink.
///
/// Configuration writes replace the directory entry itself. Treating a
/// symlink as an ordinary file would silently break the link on apply, so the
/// transaction layer rejects it and asks the user to edit the real target.
pub fn read_regular_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = open_regular_file(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    Ok(bytes)
}

pub fn read_regular_file_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = open_regular_file(path)?;
    if file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?
        .len()
        > limit
    {
        bail!("{} exceeds the {} byte limit", path.display(), limit);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!("{} exceeds the {} byte limit", path.display(), limit);
    }
    Ok(bytes)
}

pub fn open_regular_file(path: &Path) -> Result<File> {
    let (directory, name) = secure_parent_directory(path, false)?;
    let file = openat_file(
        &directory,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .with_context(|| format!("open regular file {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    reject_hard_links(path, &metadata)?;
    Ok(file)
}

pub fn read_text_or_empty(path: &Path) -> Result<String> {
    let bytes = read_bytes_or_empty(path)?;
    String::from_utf8(bytes).with_context(|| format!("decode {} as UTF-8", path.display()))
}

pub fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o7777);
    }
    #[allow(unreachable_code)]
    None
}

pub fn is_system_path(path: &Path) -> bool {
    path == Path::new("/etc")
        || path.starts_with("/etc/")
        || path == Path::new("/usr")
        || path.starts_with("/usr/")
        || path == Path::new("/var")
        || path.starts_with("/var/")
}

/// Write a single file atomically, preserving its existing mode where possible.
pub fn atomic_write(path: &Path, contents: &[u8], mode: Option<u32>) -> Result<()> {
    atomic_write_inner(path, None, false, contents, mode)
}

/// Compare and atomically replace a file while holding its verified parent
/// directory. `expected = None` means the target must not exist.
pub fn atomic_write_checked(
    path: &Path,
    expected: Option<&[u8]>,
    contents: &[u8],
    mode: Option<u32>,
) -> Result<()> {
    atomic_write_inner(path, expected, true, contents, mode)
}

fn atomic_write_inner(
    path: &Path,
    expected: Option<&[u8]>,
    enforce_expected: bool,
    contents: &[u8],
    mode: Option<u32>,
) -> Result<()> {
    let (directory, name) = secure_parent_directory(path, true)?;
    let existing = match openat_file(
        &directory,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    ) {
        Ok(file) => {
            let metadata = file
                .metadata()
                .with_context(|| format!("inspect {}", path.display()))?;
            if !metadata.is_file() {
                bail!("refusing to replace non-regular file {}", path.display());
            }
            reject_hard_links(path, &metadata)?;
            Some((file, metadata))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            bail!(
                "refusing to replace symbolic link {}; edit its resolved target explicitly",
                path.display()
            )
        }
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    if enforce_expected {
        match (expected, existing.as_ref()) {
            (Some(expected), Some((file, _))) if file_matches(file, expected)? => {}
            (Some(_), Some(_)) => bail!("{} changed before atomic replacement", path.display()),
            (Some(_), None) => bail!("{} was removed before atomic replacement", path.display()),
            (None, None) => {}
            (None, Some(_)) => bail!("{} was created before atomic replacement", path.display()),
        }
    }
    let selected_mode = mode
        .or_else(|| existing.as_ref().map(|(_, metadata)| unix_mode(metadata)))
        .unwrap_or(0o644);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary_name =
        CString::new(format!(".reginux-{nonce}.tmp")).context("temporary filename contains NUL")?;

    let result = (|| -> Result<()> {
        let mut file = openat_file(
            &directory,
            &temporary_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            selected_mode,
        )
        .with_context(|| format!("create temporary file for {}", path.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write temporary file for {}", path.display()))?;
        if let Some((source, metadata)) = &existing {
            preserve_unix_owner(&file, metadata).with_context(|| {
                format!("preserve ownership while replacing {}", path.display())
            })?;
            if unsafe { libc::fchmod(file.as_raw_fd(), selected_mode) } == -1 {
                return Err(io::Error::last_os_error()).context("set temporary file mode");
            }
            #[cfg(target_os = "linux")]
            preserve_linux_xattrs(source, &file).with_context(|| {
                format!(
                    "preserve extended attributes while replacing {}",
                    path.display()
                )
            })?;
        }

        file.sync_all().context("sync temporary file")?;
        drop(file);

        verify_directory_entry(
            &directory,
            &name,
            existing.as_ref().map(|(_, metadata)| metadata),
        )
        .with_context(|| format!("recheck {} before replacement", path.display()))?;
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary_name.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("replace {}", path.display()));
        }
        directory
            .sync_all()
            .with_context(|| format!("sync parent directory for {}", path.display()))?;
        Ok(())
    })();

    if result.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary_name.as_ptr(), 0);
        }
    }
    result
}

pub fn remove_regular_file_checked(path: &Path, expected: &[u8]) -> Result<()> {
    let (directory, name) = secure_parent_directory(path, false)?;
    let file = openat_file(
        &directory,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .with_context(|| format!("open {} for checked removal", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    reject_hard_links(path, &metadata)?;
    if !file_matches(&file, expected)? {
        bail!("{} changed before checked removal", path.display());
    }
    verify_directory_entry(&directory, &name, Some(&metadata))
        .with_context(|| format!("recheck {} before removal", path.display()))?;
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == -1 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("remove {}", path.display()));
    }
    directory
        .sync_all()
        .with_context(|| format!("sync parent directory for {}", path.display()))?;
    Ok(())
}

fn file_matches(file: &File, expected: &[u8]) -> Result<bool> {
    let mut current = Vec::new();
    file.try_clone()
        .context("clone file for content comparison")?
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut current)
        .context("read file for content comparison")?;
    Ok(current == expected)
}

fn secure_parent_directory(path: &Path, create_missing: bool) -> Result<(File, CString)> {
    use std::path::Component;

    if !path.is_absolute() {
        bail!("path must be absolute: {}", path.display());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no filename", path.display()))?;
    let name = c_component(name)?;
    let mut directory = File::open("/").context("open filesystem root")?;

    for component in parent.components() {
        let Component::Normal(component) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            bail!("path contains an unsupported component: {}", path.display());
        };
        let component = c_component(component)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        match openat_file(&directory, &component, flags, 0) {
            Ok(next) => directory = next,
            Err(error) if error.kind() == io::ErrorKind::NotFound && create_missing => {
                if unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o777) } == -1
                {
                    let create_error = io::Error::last_os_error();
                    if create_error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(create_error).with_context(|| {
                            format!("create directory component in {}", parent.display())
                        });
                    }
                }
                directory = openat_file(&directory, &component, flags, 0).with_context(|| {
                    format!("open created directory component in {}", parent.display())
                })?;
            }
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                bail!(
                    "symbolic link component is not allowed in {}",
                    path.display()
                )
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open directory component in {}", parent.display()))
            }
        }
    }
    Ok((directory, name))
}

fn c_component(component: &OsStr) -> Result<CString> {
    CString::new(component.as_bytes()).context("path component contains an interior NUL byte")
}

fn openat_file(directory: &File, name: &CString, flags: i32, mode: u32) -> io::Result<File> {
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, mode) };
    if descriptor == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn verify_directory_entry(
    directory: &File,
    name: &CString,
    expected: Option<&fs::Metadata>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let current = match openat_file(
        directory,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    ) {
        Ok(file) => Some(file.metadata().context("inspect current directory entry")?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            bail!("target became a symbolic link before apply")
        }
        Err(error) => return Err(error).context("open current directory entry"),
    };
    match (expected, current.as_ref()) {
        (Some(expected), Some(current))
            if expected.dev() == current.dev() && expected.ino() == current.ino() =>
        {
            Ok(())
        }
        (None, None) => Ok(()),
        (Some(_), None) => bail!("target was removed before apply"),
        (None, Some(_)) => bail!("target was created before apply"),
        (Some(_), Some(_)) => bail!("target was replaced before apply"),
    }
}

/// Reject symbolic links in every existing path component. Missing trailing
/// components are allowed for plans that create a new file.
pub fn reject_symlink_components(path: &Path) -> Result<()> {
    use std::path::Component;

    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!(
            "path must be absolute and contain no parent traversal: {}",
            path.display()
        );
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "symbolic link component is not allowed: {}",
                    current.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", current.display()))
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn unix_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(unix)]
fn reject_hard_links(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() > 1 {
        bail!(
            "{} has {} hard links; refusing an atomic replacement that would break link identity",
            path.display(),
            metadata.nlink()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_hard_links(_path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn preserve_unix_owner(file: &File, metadata: &fs::Metadata) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let result = unsafe { libc::fchown(file.as_raw_fd(), metadata.uid(), metadata.gid()) };
    if result == -1 {
        return Err(io::Error::last_os_error()).context("fchown temporary file");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn preserve_linux_xattrs(source: &File, destination: &File) -> Result<()> {
    let list_size = unsafe { libc::flistxattr(source.as_raw_fd(), std::ptr::null_mut(), 0) };
    if list_size == -1 {
        return Err(io::Error::last_os_error()).context("list source extended attributes");
    }
    if list_size == 0 {
        return Ok(());
    }
    let mut names = vec![0_u8; list_size as usize];
    let read_size =
        unsafe { libc::flistxattr(source.as_raw_fd(), names.as_mut_ptr().cast(), names.len()) };
    if read_size == -1 {
        return Err(io::Error::last_os_error()).context("read source extended attributes");
    }
    names.truncate(read_size as usize);

    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = CString::new(name).context("extended attribute name contains NUL")?;
        let value_size =
            unsafe { libc::fgetxattr(source.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if value_size == -1 {
            return Err(io::Error::last_os_error()).context("measure extended attribute");
        }
        let mut value = vec![0_u8; value_size as usize];
        let value_ptr = if value.is_empty() {
            std::ptr::null_mut()
        } else {
            value.as_mut_ptr().cast()
        };
        let read_value =
            unsafe { libc::fgetxattr(source.as_raw_fd(), name.as_ptr(), value_ptr, value.len()) };
        if read_value == -1 {
            return Err(io::Error::last_os_error()).context("read extended attribute");
        }
        value.truncate(read_value as usize);
        let value_ptr = if value.is_empty() {
            std::ptr::null()
        } else {
            value.as_ptr().cast()
        };
        let set_result = unsafe {
            libc::fsetxattr(
                destination.as_raw_fd(),
                name.as_ptr(),
                value_ptr,
                value.len(),
                0,
            )
        };
        if set_result == -1 {
            return Err(io::Error::last_os_error()).context("write extended attribute");
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn preserve_unix_owner(_file: &File, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

pub fn make_working_copy(path: &Path, contents: &[u8]) -> Result<PathBuf> {
    let mut working = env::temp_dir();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    working.push(format!("reginux-{nonce}-{name}"));
    atomic_write(&working, contents, Some(0o600))?;
    Ok(working)
}

pub fn parse_editor_command(
    config: &EditorConfig,
    file: &Path,
) -> Result<(OsString, Vec<OsString>)> {
    let configured = if config.use_environment_editor {
        env::var("VISUAL")
            .ok()
            .or_else(|| env::var("EDITOR").ok())
            .unwrap_or_else(|| config.command.clone())
    } else {
        config.command.clone()
    };
    let configured = if configured.trim().is_empty() {
        "vim {file}".to_owned()
    } else {
        configured
    };
    let tokens = shell_words::split(&configured)
        .map_err(|error| anyhow!("invalid editor command: {error}"))?;
    let mut tokens = tokens.into_iter();
    let program = tokens
        .next()
        .ok_or_else(|| anyhow!("editor command is empty"))?;
    let file = file.as_os_str().to_os_string();
    let mut inserted_file = false;
    let mut args = tokens
        .map(|token| {
            if token == "{file}" {
                inserted_file = true;
                file.clone()
            } else {
                OsString::from(token)
            }
        })
        .collect::<Vec<_>>();
    if !inserted_file {
        args.push(file);
    }
    Ok((OsString::from(program), args))
}

pub fn run_editor(config: &EditorConfig, file: &Path) -> Result<()> {
    let (program, args) = parse_editor_command(config, file)?;
    let status = Command::new(&program)
        .args(args)
        .status()
        .with_context(|| format!("start editor {}", program.to_string_lossy().into_owned()))?;
    if !status.success() {
        bail!("editor exited with status {status}");
    }
    Ok(())
}

pub fn backup_root_for(path: &Path) -> PathBuf {
    if is_system_path(path) {
        PathBuf::from("/var/lib/reginux/backups")
    } else {
        state_dir().join("backups")
    }
}

pub fn backup_name(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut name = raw
        .trim_start_matches('/')
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-') {
                ch
            } else {
                '_'
            }
        })
        .take(120)
        .collect::<String>();
    if name.is_empty() {
        name = "root".to_owned();
    }
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{name}-{:016x}", hasher.finish())
}

pub fn write_backup(transaction_id: &str, path: &Path, contents: &[u8]) -> Result<PathBuf> {
    let directory = backup_root_for(path).join(transaction_id);
    fs::create_dir_all(&directory)
        .with_context(|| format!("create backup directory {}", directory.display()))?;
    let destination = directory.join(backup_name(path));
    atomic_write(&destination, contents, Some(0o600))?;
    Ok(destination)
}

pub fn current_uid_is_root() -> bool {
    #[cfg(unix)]
    {
        return unsafe { libc_getuid() == 0 };
    }
    #[allow(unreachable_code)]
    false
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}
