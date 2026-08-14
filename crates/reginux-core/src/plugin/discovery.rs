use super::*;

pub fn discover_plugins(directories: &[String], policy: &PluginPolicy) -> PluginDiscovery {
    let mut entries: Vec<ConfigEntry> = Vec::new();
    let mut summaries: Vec<PluginSummary> = Vec::new();
    let mut diagnostics = Vec::new();
    let mut candidates = Vec::new();

    for configured in directories {
        let root = expand_configured_root(configured);
        if !root.exists() {
            continue;
        }
        match secure_plugin_directories(&root) {
            Ok(mut directories) => candidates.append(&mut directories),
            Err(error) => diagnostics.push(format!("plugin root {}: {error}", root.display())),
        }
    }
    candidates.sort();
    candidates.dedup();
    if candidates.len() > MAX_PLUGIN_CANDIDATES {
        diagnostics.push(format!(
            "plugin candidate limit reached; only the first {MAX_PLUGIN_CANDIDATES} directories were inspected"
        ));
        candidates.truncate(MAX_PLUGIN_CANDIDATES);
    }

    let mut seen_ids: HashMap<String, PathBuf> = HashMap::new();
    let mut conflicted_ids = HashSet::new();
    for chunk in candidates.chunks(PLUGIN_DISCOVERY_CONCURRENCY) {
        let loaded = thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|directory| {
                    scope.spawn(move || {
                        let manifest = find_manifest(directory)
                            .with_context(|| format!("inspect {}", directory.display()))?;
                        Ok::<_, anyhow::Error>(manifest.map(|manifest_path| {
                            let result = load_plugin(directory, &manifest_path, policy);
                            (directory.clone(), manifest_path, result)
                        }))
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join())
                .collect::<Vec<_>>()
        });
        for candidate in loaded {
            let candidate = match candidate {
                Ok(Ok(candidate)) => candidate,
                Ok(Err(error)) => {
                    diagnostics.push(format!("plugin discovery: {error}"));
                    continue;
                }
                Err(_) => {
                    diagnostics.push("plugin discovery worker panicked".to_owned());
                    continue;
                }
            };
            let Some((directory, manifest_path, result)) = candidate else {
                continue;
            };
            match result {
                Ok((manifest_id, mut plugin_entries, summary)) => {
                    if let Some(first) = seen_ids.get(&manifest_id) {
                        diagnostics.push(format!(
                            "duplicate plugin id {manifest_id}: {} conflicts with {}; all copies were disabled",
                            directory.display(),
                            first.display()
                        ));
                        conflicted_ids.insert(manifest_id.clone());
                        entries.retain(|entry| entry.provider != format!("plugin.{manifest_id}"));
                        if let Some(previous) =
                            summaries.iter_mut().find(|plugin| plugin.id == manifest_id)
                        {
                            previous.status = "disabled; duplicate plugin id".to_owned();
                        }
                        summaries.push(PluginSummary {
                            status: format!("disabled; duplicate of {}", first.display()),
                            ..summary
                        });
                    } else {
                        seen_ids.insert(manifest_id, directory.clone());
                        if !conflicted_ids.contains(&summary.id) {
                            entries.append(&mut plugin_entries);
                        }
                        summaries.push(summary);
                    }
                }
                Err(error) => summaries.push(error_summary(&manifest_path, error)),
            }
        }
    }

    PluginDiscovery {
        entries,
        summaries,
        diagnostics,
    }
}

pub(super) fn load_plugin(
    directory: &Path,
    manifest_path: &Path,
    policy: &PluginPolicy,
) -> Result<(String, Vec<ConfigEntry>, PluginSummary)> {
    let bytes = read_limited_regular_file(manifest_path, MANIFEST_LIMIT)?;
    let text = std::str::from_utf8(&bytes).context("manifest is not UTF-8")?;
    let manifest: Manifest = toml::from_str(text).context("parse plugin manifest TOML")?;
    validate_manifest(&manifest)?;
    let digest = plugin_digest(directory, &manifest, &bytes)?;
    let is_system_install = directory.starts_with("/usr/share/reginux/plugins");
    if is_system_install
        || manifest
            .sources
            .values()
            .any(|source| source.scope == ScopeSpec::System)
    {
        validate_system_plugin_location(directory, manifest_path)?;
    }
    let approved = is_approved(&manifest, &digest, policy);

    let (mut entries, status, permissions, sources, capabilities, approval) =
        match manifest.plugin.kind {
            PluginKind::Schema => {
                let entries = load_schema(directory, &manifest)?;
                (
                    entries,
                    format!("loaded ({})", manifest.plugin.version),
                    vec!["declarative; no code execution".to_owned()],
                    source_descriptions(&manifest.sources),
                    vec!["read".to_owned(), "file-plan".to_owned()],
                    "not required".to_owned(),
                )
            }
            PluginKind::Adapter if !approved => (
                Vec::new(),
                "disabled; adapter approval required".to_owned(),
                adapter_permissions(&manifest),
                source_descriptions(&manifest.sources),
                adapter_capabilities(&manifest),
                "required".to_owned(),
            ),
            PluginKind::Adapter => (
                load_adapter(directory, &manifest, policy.refresh_runtime)?,
                format!("loaded ({})", manifest.plugin.version),
                adapter_permissions(&manifest),
                source_descriptions(&manifest.sources),
                adapter_capabilities(&manifest),
                "approved".to_owned(),
            ),
            PluginKind::Transform if !approved => (
                Vec::new(),
                "disabled; transform approval required".to_owned(),
                vec!["sandboxed Lua transform".to_owned()],
                source_descriptions(&manifest.sources),
                vec!["decode".to_owned(), "file-plan".to_owned()],
                "required".to_owned(),
            ),
            PluginKind::Transform => {
                let (entries, transform_diagnostics) = load_transform(directory, &manifest)?;
                let status = if transform_diagnostics.is_empty() {
                    format!("loaded ({})", manifest.plugin.version)
                } else {
                    format!(
                        "loaded ({}) with diagnostics: {}",
                        manifest.plugin.version,
                        transform_diagnostics.join("; ")
                    )
                };
                (
                    entries,
                    status,
                    vec![
                        "sandboxed Lua; no I/O, process, network, package or debug libraries"
                            .to_owned(),
                    ],
                    source_descriptions(&manifest.sources),
                    vec!["decode".to_owned(), "file-plan".to_owned()],
                    "approved".to_owned(),
                )
            }
        };

    for entry in &mut entries {
        if entry.source.is_system() {
            entry.metadata.push((
                "system_plugin_manifest".to_owned(),
                manifest_path.display().to_string(),
            ));
            entry
                .metadata
                .push(("system_plugin_digest".to_owned(), digest.clone()));
            entry
                .metadata
                .push(("system_plugin_id".to_owned(), manifest.plugin.id.clone()));
        }
    }

    let id = manifest.plugin.id.clone();
    Ok((
        id.clone(),
        entries,
        PluginSummary {
            id,
            name: clean_text(&manifest.plugin.name),
            kind: manifest.plugin.kind.as_str().to_owned(),
            path: directory.to_path_buf(),
            status,
            permissions,
            trust: plugin_trust(directory, &manifest.plugin.kind),
            approval,
            digest: Some(digest),
            sources,
            capabilities,
            captured_at: (manifest.plugin.kind == PluginKind::Adapter && approved)
                .then(|| Utc::now().to_rfc3339()),
            last_error: None,
            last_error_at: None,
            stale: false,
        },
    ))
}

/// Revalidate a system Schema plugin at the privileged boundary and confirm
/// that `target` is one of the files reached from its declared source graph.
/// User-installed manifests are deliberately never trusted for system writes.
pub fn authorize_system_schema_target(
    manifest_path: &Path,
    expected_plugin_id: &str,
    expected_digest: &str,
    target: &Path,
) -> Result<()> {
    let system_root = Path::new("/usr/share/reginux/plugins");
    if !manifest_path.starts_with(system_root) {
        bail!("system-writing plugin manifests must be installed under /usr/share/reginux/plugins");
    }
    let directory = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("plugin manifest has no parent directory"))?;
    validate_system_plugin_location(directory, manifest_path)?;
    let bytes = read_limited_regular_file(manifest_path, MANIFEST_LIMIT)?;
    let text = std::str::from_utf8(&bytes).context("manifest is not UTF-8")?;
    let manifest: Manifest = toml::from_str(text).context("parse plugin manifest TOML")?;
    validate_manifest(&manifest)?;
    if manifest.plugin.kind != PluginKind::Schema || manifest.plugin.id != expected_plugin_id {
        bail!("privileged plugin identity or kind does not match the staged plan");
    }
    let digest = plugin_digest(directory, &manifest, &bytes)?;
    if digest != expected_digest {
        bail!("system plugin changed after discovery; privileged write refused");
    }
    let documents = resolve_documents(&manifest.sources)?;
    let declared = documents
        .iter()
        .any(|document| document.scope == SourceScope::System && document.path.as_path() == target);
    if !declared {
        bail!("target is not a declared system source of the approved plugin");
    }
    Ok(())
}
