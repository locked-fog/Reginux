use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use reginux_core::config::{EditorConfig, KeybindingsConfig};
use reginux_core::filesystem::{
    atomic_write, atomic_write_checked, backup_name, parse_editor_command,
    remove_regular_file_checked,
};
use reginux_core::keybindings::{parse_key_sequence, Keymap};
use reginux_core::model::{
    Backend, ConfigEntry, EditCapability, KeyValueSeparator, Privilege, SourceRef, SourceScope,
    ValueType,
};
use reginux_core::Transaction;

fn fixture_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "reginux-test-{}-{nonce}-{name}",
        std::process::id()
    ))
}

#[test]
fn default_keymap_supports_vim_sequence_prefix() {
    let keymap = Keymap::default();
    let sequence = parse_key_sequence("g").unwrap();
    assert!(keymap.is_prefix("browser", &sequence.0));
    let full = parse_key_sequence("gg").unwrap();
    assert_eq!(
        keymap.resolve("browser", &full.0).unwrap().0,
        "navigation.top"
    );
}

#[test]
fn shifted_printable_terminal_events_match_literal_bindings() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use reginux_core::keybindings::KeyStroke;

    let event = KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT);
    let actual = KeyStroke::from_event(event);
    assert_eq!(actual, parse_key_sequence("Q").unwrap().0[0]);

    let event = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
    let actual = KeyStroke::from_event(event);
    assert_eq!(actual, parse_key_sequence("?").unwrap().0[0]);
}

#[test]
fn key_value_staging_keeps_comments_and_updates_one_field() {
    let path = fixture_path("locale.conf");
    fs::write(&path, b"# keep this comment\nLANG=zh_CN.UTF-8\nLC_TIME=C\n").unwrap();
    let entry = ConfigEntry {
        id: "test.locale.lang".to_owned(),
        label: "LANG".to_owned(),
        section: "Test".to_owned(),
        description: String::new(),
        value: "zh_CN.UTF-8".to_owned(),
        default_value: None,
        value_type: ValueType::String,
        source: SourceRef::file("test", path.clone(), SourceScope::User),
        edit_capability: EditCapability::File,
        privilege: Privilege::User,
        provider: "test".to_owned(),
        validation: "string".to_owned(),
        backend: Backend::KeyValue {
            path: path.clone(),
            key: "LANG".to_owned(),
            separator: KeyValueSeparator::Equals,
        },
        metadata: vec![],
    };
    let mut transaction = Transaction::default();
    transaction.stage_entry(&entry, "en_US.UTF-8").unwrap();
    let content = String::from_utf8(transaction.content_for(&path).unwrap()).unwrap();
    assert!(content.contains("# keep this comment"));
    assert!(content.contains("LANG=en_US.UTF-8"));
    assert!(content.contains("LC_TIME=C"));
    assert!(transaction.diff().contains("-LANG=zh_CN.UTF-8"));
    let _ = fs::remove_file(path);
}

#[test]
fn invalid_staged_bytes_block_apply_without_touching_source() {
    let path = fixture_path("invalid.conf");
    fs::write(&path, b"original\n").unwrap();

    let mut transaction = Transaction::default();
    transaction
        .stage_raw(&path, b"invalid\0\n".to_vec())
        .unwrap();

    let issues = transaction.validate();
    assert_eq!(issues.len(), 1);
    assert!(transaction.apply(false).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"original\n");
    let _ = fs::remove_file(path);
}

#[test]
fn public_staging_rejects_relative_and_out_of_scope_paths() {
    let mut transaction = Transaction::default();
    assert!(transaction
        .stage_raw(Path::new("relative.conf"), b"blocked\n".to_vec())
        .unwrap_err()
        .to_string()
        .contains("absolute"));
    assert!(transaction
        .stage_raw(
            Path::new("/opt/reginux-out-of-scope.conf"),
            b"blocked\n".to_vec()
        )
        .unwrap_err()
        .to_string()
        .contains("outside HOME"));
    assert!(transaction
        .stage_raw(Path::new("/etc/reginux-system.conf"), b"blocked\n".to_vec())
        .unwrap_err()
        .to_string()
        .contains("system paths"));
}

#[test]
fn invalid_reginux_config_is_blocked_before_apply() {
    let root = fixture_path("invalid-reginux-config");
    let config_directory = root.join("reginux");
    fs::create_dir_all(&config_directory).unwrap();
    let path = config_directory.join("config.toml");
    fs::write(&path, "[interface]\ndefault_view = \"form\"\n").unwrap();

    let mut transaction = Transaction::default();
    transaction
        .stage_raw(&path, b"[interface\ndefault_view = 42\n".to_vec())
        .unwrap();
    let error = transaction
        .apply(true)
        .expect_err("invalid Reginux TOML must not be written");
    assert!(error.to_string().contains("invalid Reginux configuration"));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "[interface]\ndefault_view = \"form\"\n"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn apply_writes_staged_file_and_clears_transaction() {
    let path = fixture_path("apply.conf");
    fs::write(&path, b"before\n").unwrap();

    let mut transaction = Transaction::default();
    transaction.stage_raw(&path, b"after\n".to_vec()).unwrap();
    let report = transaction.apply(false).unwrap();

    assert_eq!(report.files, vec![path.clone()]);
    assert_eq!(fs::read(&path).unwrap(), b"after\n");
    assert_eq!(transaction.changed_count(), 0);
    let _ = fs::remove_file(path);
}

#[test]
fn atomic_write_replaces_file_and_uses_requested_mode() {
    let path = fixture_path("atomic.conf");
    atomic_write(&path, b"written\n", Some(0o600)).unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"written\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let _ = fs::remove_file(path);
}

#[test]
fn checked_replacement_and_removal_preserve_external_changes() {
    let path = fixture_path("checked-write.conf");
    fs::write(&path, b"external\n").unwrap();

    assert!(atomic_write_checked(&path, Some(b"staged\n"), b"replacement\n", None).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"external\n");
    assert!(remove_regular_file_checked(&path, b"staged\n").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"external\n");
    fs::remove_file(path).unwrap();
}

#[cfg(unix)]
#[test]
fn file_transactions_reject_symlinked_directory_components() {
    use std::os::unix::fs::symlink;

    let root = fixture_path("symlink-parent");
    let real = root.join("real");
    let linked = root.join("linked");
    fs::create_dir_all(&real).unwrap();
    symlink(&real, &linked).unwrap();
    let target = linked.join("settings.conf");

    let error = atomic_write(&target, b"unsafe\n", Some(0o600))
        .unwrap_err()
        .to_string();
    assert!(error.contains("symbolic link") || error.contains("directory component"));
    assert!(!real.join("settings.conf").exists());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn atomic_write_preserves_linux_extended_attributes() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let root = fixture_path("xattr");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("settings.conf");
    fs::write(&path, "before\n").unwrap();
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new("user.reginux-test").unwrap();
    let expected = b"preserved";
    let result = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            expected.as_ptr().cast(),
            expected.len(),
            0,
        )
    };
    assert_eq!(result, 0, "test filesystem must support user xattrs");

    atomic_write(&path, b"after\n", None).unwrap();
    let mut actual = vec![0_u8; expected.len()];
    let read = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            actual.as_mut_ptr().cast(),
            actual.len(),
        )
    };
    assert_eq!(read, expected.len() as isize);
    assert_eq!(actual, expected);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn keymap_rejects_prefix_and_unknown_action_conflicts() {
    let mut prefix_config = KeybindingsConfig::default();
    prefix_config
        .browser
        .insert("g".to_owned(), "navigation.top".to_owned());
    assert!(Keymap::from_config(&prefix_config).is_err());

    let mut action_config = KeybindingsConfig::default();
    action_config
        .global
        .insert("x".to_owned(), "not.an.action".to_owned());
    assert!(Keymap::from_config(&action_config).is_err());
}

#[test]
fn layered_keymap_prefers_the_first_active_context() {
    let mut config = KeybindingsConfig::default();
    config.browser.insert("e".to_owned(), "view.raw".to_owned());
    config.form.insert("e".to_owned(), "config.edit".to_owned());
    let keymap = Keymap::from_config(&config).unwrap();
    let sequence = parse_key_sequence("e").unwrap();
    assert_eq!(
        keymap.resolve_contexts(&["form", "browser"], &sequence.0),
        Some(reginux_core::keybindings::Action::new("config.edit"))
    );
}

#[test]
fn keymap_rejects_prefix_conflicts_across_active_layers() {
    let mut config = KeybindingsConfig::default();
    config.global.insert("g".to_owned(), "view.form".to_owned());
    assert!(Keymap::from_config(&config).is_err());
}

#[test]
fn apply_rejects_external_changes_and_keeps_staged_state() {
    let path = fixture_path("conflict.conf");
    fs::write(&path, b"original\n").unwrap();

    let mut transaction = Transaction::default();
    transaction.stage_raw(&path, b"staged\n".to_vec()).unwrap();
    fs::write(&path, b"changed elsewhere\n").unwrap();

    let error = transaction.apply(false).unwrap_err().to_string();
    assert!(error.contains("source conflict"));
    assert_eq!(fs::read(&path).unwrap(), b"changed elsewhere\n");
    assert_eq!(transaction.changed_count(), 1);
    let _ = fs::remove_file(path);
}

#[cfg(unix)]
#[test]
fn staging_rejects_symbolic_link_targets() {
    use std::os::unix::fs::symlink;

    let target = fixture_path("link-target.conf");
    let link = fixture_path("link.conf");
    fs::write(&target, b"target\n").unwrap();
    symlink(&target, &link).unwrap();

    let mut transaction = Transaction::default();
    let error = transaction
        .stage_raw(&link, b"replacement\n".to_vec())
        .unwrap_err()
        .to_string();
    assert!(error.contains("symbolic link"));
    assert_eq!(fs::read(&target).unwrap(), b"target\n");
    let _ = fs::remove_file(link);
    let _ = fs::remove_file(target);
}

#[cfg(unix)]
#[test]
fn staging_rejects_hard_link_targets() {
    let root = fixture_path("hard-link");
    fs::create_dir_all(&root).unwrap();
    let source = root.join("source.conf");
    let alias = root.join("alias.conf");
    fs::write(&source, "before\n").unwrap();
    fs::hard_link(&source, &alias).unwrap();

    let mut transaction = Transaction::default();
    let error = transaction
        .stage_raw(&source, b"after\n".to_vec())
        .expect_err("hard-linked targets must be rejected");
    assert!(error.to_string().contains("hard links"));
    assert_eq!(fs::read_to_string(&alias).unwrap(), "before\n");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn structured_edit_preserves_inline_comment_quotes_and_earlier_duplicate() {
    let path = fixture_path("comments.conf");
    fs::write(
        &path,
        b"KEY=earlier\nKEY = \"old value\"  # keep this comment\nOTHER=yes\n",
    )
    .unwrap();
    let entry = test_key_value_entry(&path, "test.key", "KEY", "old value");

    let mut transaction = Transaction::default();
    transaction.stage_entry(&entry, "new value").unwrap();
    let content = String::from_utf8(transaction.content_for(&path).unwrap()).unwrap();

    assert!(content.contains("KEY=earlier"));
    assert!(content.contains("KEY = \"new value\"  # keep this comment"));
    assert!(content.contains("OTHER=yes"));
    let _ = fs::remove_file(path);
}

#[test]
fn locale_inherit_removes_assignment_instead_of_writing_placeholder() {
    let path = fixture_path("locale-inherit.conf");
    fs::write(&path, b"# locale\nLANG=en_US.UTF-8\nLC_TIME=C\n").unwrap();
    let entry = test_key_value_entry(&path, "linux.locale.LANG", "LANG", "en_US.UTF-8");

    let mut transaction = Transaction::default();
    transaction.stage_entry(&entry, "<inherit>").unwrap();
    let content = String::from_utf8(transaction.content_for(&path).unwrap()).unwrap();

    assert_eq!(content, "# locale\nLC_TIME=C\n");
    let _ = fs::remove_file(path);
}

#[test]
fn toml_boolean_alias_is_normalized_and_result_is_parseable() {
    let path = fixture_path("config.toml");
    fs::write(
        &path,
        b"[interface]\nshow_key_hints = false\n\n[editor]\ncommand = \"vim {file}\"\n",
    )
    .unwrap();
    let entry = ConfigEntry {
        id: "reginux.interface.show_key_hints".to_owned(),
        label: "Show hints".to_owned(),
        section: "Reginux / Interface".to_owned(),
        description: String::new(),
        value: "false".to_owned(),
        default_value: None,
        value_type: ValueType::Boolean,
        source: SourceRef::file("test", path.clone(), SourceScope::User),
        edit_capability: EditCapability::File,
        privilege: Privilege::User,
        provider: "test".to_owned(),
        validation: "boolean".to_owned(),
        backend: Backend::TomlField {
            path: path.clone(),
            section: Some("interface".to_owned()),
            key: "show_key_hints".to_owned(),
            value_type: ValueType::Boolean,
        },
        metadata: vec![],
    };

    let mut transaction = Transaction::default();
    transaction.stage_entry(&entry, "yes").unwrap();
    let content = String::from_utf8(transaction.content_for(&path).unwrap()).unwrap();
    assert!(content.contains("show_key_hints = true"));
    toml::from_str::<toml::Value>(&content).unwrap();
    let _ = fs::remove_file(path);
}

#[test]
fn editor_command_appends_file_when_placeholder_is_omitted() {
    let config = EditorConfig {
        command: "nvim --clean".to_owned(),
        use_environment_editor: false,
    };
    let path = PathBuf::from("/tmp/file with spaces.conf");
    let (program, args) = parse_editor_command(&config, &path).unwrap();
    assert_eq!(program, "nvim");
    assert_eq!(args.last().unwrap(), path.as_os_str());
}

#[test]
fn backup_names_do_not_collapse_distinct_paths() {
    assert_ne!(
        backup_name(PathBuf::from("/a_b/c").as_path()),
        backup_name(PathBuf::from("/a/b_c").as_path())
    );
}

fn test_key_value_entry(path: &Path, id: &str, key: &str, value: &str) -> ConfigEntry {
    ConfigEntry {
        id: id.to_owned(),
        label: key.to_owned(),
        section: "Test".to_owned(),
        description: String::new(),
        value: value.to_owned(),
        default_value: None,
        value_type: ValueType::String,
        source: SourceRef::file("test", path.to_path_buf(), SourceScope::User),
        edit_capability: EditCapability::File,
        privilege: Privilege::User,
        provider: "test".to_owned(),
        validation: "string".to_owned(),
        backend: Backend::KeyValue {
            path: path.to_path_buf(),
            key: key.to_owned(),
            separator: KeyValueSeparator::Equals,
        },
        metadata: vec![],
    }
}
