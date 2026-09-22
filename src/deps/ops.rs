//! The `lait deps` command implementations — `add`/`install`/`update`
//! (async: they fetch from GitHub through [`GitHubClient`]) and `remove`/
//! `list`/`verify` (synchronous: manifest/lock/local files only) — plus the
//! [`RegistryEntries`] computation `config::load` merges into
//! `workflows:`/`agents:`/`skills:` so an installed dependency is reachable
//! by name (`lait run <NAME>`, `--subagent <NAME>`, `skills: [NAME]`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{agent, report, skill, storage, workflow};

use super::{
    github::GitHubClient,
    lock::{LockFile, LockedDep, load_lock, save_lock},
    manifest::{
        DepRequirement, DepsManifest, ManifestLocation, load_manifest, load_manifest_cancellable,
        save_manifest,
    },
    spec::{self, DepKind, GitHubSpec},
};

/// The payload file a dep is materialized at, relative to the manifest's
/// directory — `.lait/deps/<name>/<basename>`, keeping the upstream file
/// name so `lait workflow list`-style output still reads naturally. Shared
/// by the writers (`add`/`install`/`update`) and the config-registry merge
/// so both agree on where "installed" means.
fn payload_relpath(name: &str, spec: &GitHubSpec) -> Result<PathBuf> {
    let basename = spec::path_basename(&spec.path)
        .context("internal error: a validated source path has no basename")?;
    Ok(Path::new(".lait").join("deps").join(name).join(basename))
}

/// Writes a dep's payload to disk: clears `.lait/deps/<name>/` first so a
/// dep whose source path changed doesn't leave a stale sibling file, then
/// publishes the new content atomically (`storage::write_atomic`, the same
/// primitive cache/checkpoint snapshots use). The directory is entirely
/// tool-managed — regenerable from `lait.lock` — so clearing it loses
/// nothing.
fn write_payload(
    location: &ManifestLocation,
    name: &str,
    spec: &GitHubSpec,
    content: &[u8],
) -> Result<String> {
    let dir = location.dep_dir(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to clear '{}'", dir.display()))?;
    }
    let relative = payload_relpath(name, spec)?;
    let absolute = location.dir.join(&relative);
    storage::write_atomic(&absolute, content)?;
    // `to_string_lossy` + `/`-joining: the lock stores a portable
    // forward-slash relative path (it is meant to be committed); the
    // components here are a fixed ASCII prefix plus a validated name and
    // basename, so no platform separator ambiguity arises in practice.
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

/// The parse check `add`/`install`/`update` run on fetched bytes before
/// anything is written: a dep that doesn't parse as its declared kind is
/// rejected at the boundary, so `lait run <name>` can't fail later with a
/// confusing parse error pointing at `.lait/deps/`.
fn validate_payload(kind: DepKind, name: &str, content: &[u8]) -> Result<()> {
    let context = || format!("dependency '{name}' is not a valid {} file", kind.name());
    let text = std::str::from_utf8(content).with_context(|| {
        format!(
            "dependency '{name}' is not UTF-8, so not a valid {} file",
            kind.name()
        )
    })?;
    match kind {
        DepKind::Workflow => workflow::parse_workflow(text).map(|_| ()),
        DepKind::Agent => agent::parse_agent(text).map(|_| ()),
        DepKind::Skill => skill::validate_skill(name, text),
    }
    .with_context(context)
}

/// Whether the materialized payload on disk still matches its lock entry.
enum PayloadStatus {
    Installed,
    Missing,
    Modified,
}

fn payload_status(location: &ManifestLocation, locked: &LockedDep) -> PayloadStatus {
    let file = locked.absolute_file(location);
    if !file.is_file() {
        return PayloadStatus::Missing;
    }
    match LockFile::sha256_file(&file) {
        Ok(digest) if digest == locked.sha256 => PayloadStatus::Installed,
        _ => PayloadStatus::Modified,
    }
}

fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// The manifest `deps` commands operate on: the one found by walking
/// ancestor directories (`load_manifest_cancellable`'s rule), or — only for
/// `add`, which is allowed to bootstrap — a fresh one rooted at the current
/// directory. Every other command takes [`load_manifest_cancellable`]'s
/// `Option` directly, since a missing manifest means "no dependencies
/// declared", not "create one".
async fn manifest_for_add(
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ManifestLocation> {
    if let Some(location) = load_manifest_cancellable(cancel.clone()).await? {
        return Ok(location);
    }
    let cwd = std::env::current_dir()
        .context("failed to determine the current directory for 'lait deps add'")?;
    Ok(ManifestLocation {
        dir: cwd,
        manifest: DepsManifest::default(),
    })
}

/// `lait deps add <SPEC>`: resolve the ref, download the file, validate it
/// as its kind, materialize it under `.lait/deps/<name>/`, then record the
/// request in `lait.deps.yml` and the resolution in `lait.lock`. Nothing is
/// written before the fetch succeeds and validates, so a failed add leaves
/// the project untouched.
pub(crate) async fn run_add(
    args: crate::cli::DepsAddArgs,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    let mut spec = spec::parse(&args.spec)?;
    if let Some(git_ref) = args.git_ref {
        spec.git_ref = Some(git_ref);
    }
    let name = match args.name {
        Some(name) => {
            spec::validate_name(&name)?;
            name
        }
        None => spec::derive_name(&spec.path)?,
    };
    let kind = match args.kind {
        Some(kind) => kind,
        None => DepKind::infer(&spec.path).with_context(|| {
            format!(
                "cannot infer a kind for '{name}' from '{}'; pass --kind workflow|agent|skill",
                spec.path
            )
        })?,
    };
    let mut location = manifest_for_add(&cancel).await?;
    if location.manifest.deps.contains_key(&name) {
        bail!(
            "dependency '{name}' already exists in '{}'; use `lait deps update {name}` to move it or `lait deps remove {name}` first",
            location.manifest_path().display()
        );
    }

    let client = GitHubClient::new();
    let fetched = client.fetch(&spec, &cancel).await?;
    validate_payload(kind, &name, &fetched.content)?;
    let file = write_payload(&location, &name, &spec, &fetched.content)?;

    location.manifest.deps.insert(
        name.clone(),
        DepRequirement {
            source: spec.to_source(),
            git_ref: spec.git_ref.clone(),
            kind: Some(kind),
        },
    );
    save_manifest(&location)?;

    let mut lock = load_lock(&location)?;
    lock.deps.insert(
        name.clone(),
        LockedDep {
            kind,
            repo: spec.repo_slug(),
            path: spec.path.clone(),
            git_ref: spec.git_ref.clone(),
            commit: fetched.commit,
            sha256: LockFile::sha256_hex(&fetched.content),
            file,
        },
    );
    save_lock(&location, &lock)?;

    println!(
        "added {name} ({}) {}/{}@{} = {} -> {}",
        kind.name(),
        spec.repo_slug(),
        spec.path,
        spec.git_ref.as_deref().unwrap_or("<default branch>"),
        short_commit(&lock.deps[&name].commit),
        location.dir.join(&lock.deps[&name].file).display(),
    );
    match kind {
        DepKind::Workflow => println!("run it with: lait run {name}"),
        DepKind::Agent => println!(
            "run it with: lait agent run {name} — or expose it as a tool via 'subagents: [{name}]'"
        ),
        DepKind::Skill => {
            println!(
                "use it by listing '{name}' in a 'skills:' entry (agent file, workflow node, or default.skills)"
            )
        }
    }
    Ok(())
}

/// `lait deps install [--frozen]`: materialize every manifest entry under
/// `.lait/deps/` per `lait.lock` — fetching at the locked commit when the
/// lock still covers the request, re-resolving (and updating the lock) only
/// when the manifest moved. `--frozen` turns that second path into an
/// error, the CI answer to "install exactly what the lock says". Entries
/// dropped from the manifest are pruned from both the lock and
/// `.lait/deps/`.
pub(crate) async fn run_install(
    args: crate::cli::DepsInstallArgs,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    let Some(location) = load_manifest_cancellable(cancel.clone()).await? else {
        println!(
            "no {} found; nothing to install",
            super::manifest::MANIFEST_FILE_NAME
        );
        return Ok(());
    };
    let lock_path = location.lock_path();
    if args.frozen && !lock_path.is_file() {
        bail!(
            "--frozen was given but '{}' does not exist; run `lait deps install` to create it",
            lock_path.display()
        );
    }
    let mut lock = load_lock(&location)?;
    let client = GitHubClient::new();
    let mut lock_changed = false;

    for (name, req) in location.manifest.deps.clone() {
        let (spec, kind) = req.resolve(&name)?;
        let locked = lock.deps.get(&name);
        match locked {
            Some(locked) if locked.matches(&spec, kind) => {
                match payload_status(&location, locked) {
                    PayloadStatus::Installed => {
                        println!("{name}: up to date ({})", short_commit(&locked.commit));
                    }
                    status @ (PayloadStatus::Missing | PayloadStatus::Modified) => {
                        // Refetch at the *locked* commit — never re-resolve
                        // the ref, that is the entire point of the lock —
                        // and check the bytes against the locked digest, so
                        // a corrupted transport can't silently replace what
                        // `lait.lock` pinned.
                        let content = client
                            .fetch_file(&locked.repo, &locked.commit, &locked.path, &cancel)
                            .await?;
                        if LockFile::sha256_hex(&content) != locked.sha256 {
                            bail!(
                                "fetched content for '{name}' does not match its sha256 in lait.lock"
                            );
                        }
                        validate_payload(locked.kind, &name, &content)?;
                        write_payload(&location, &name, &spec, &content)?;
                        let verb = if matches!(status, PayloadStatus::Modified) {
                            "restored"
                        } else {
                            "installed"
                        };
                        println!("{name}: {verb} ({})", short_commit(&locked.commit));
                    }
                }
            }
            _ => {
                if args.frozen {
                    bail!(
                        "'{name}' in {} is not covered by {} (the request changed or was never locked); \
                         run `lait deps install` or `lait deps update` to update the lock",
                        location.manifest_path().display(),
                        lock_path.display(),
                    );
                }
                let fetched = client.fetch(&spec, &cancel).await?;
                validate_payload(kind, &name, &fetched.content)?;
                let file = write_payload(&location, &name, &spec, &fetched.content)?;
                lock.deps.insert(
                    name.clone(),
                    LockedDep {
                        kind,
                        repo: spec.repo_slug(),
                        path: spec.path.clone(),
                        git_ref: spec.git_ref.clone(),
                        commit: fetched.commit.clone(),
                        sha256: LockFile::sha256_hex(&fetched.content),
                        file,
                    },
                );
                lock_changed = true;
                println!("{name}: installed ({})", short_commit(&fetched.commit));
            }
        }
    }

    // Prune: lock entries and `.lait/deps/` directories that no manifest
    // entry covers anymore (a `remove` interrupted mid-write, a reverted
    // edit, ...). `remove_dir_all` is safe here for the same reason as in
    // `write_payload` — everything under `deps_dir` is regenerable.
    let stale: Vec<String> = lock
        .deps
        .keys()
        .filter(|name| !location.manifest.deps.contains_key(*name))
        .cloned()
        .collect();
    for name in stale {
        lock.deps.remove(&name);
        lock_changed = true;
        let dir = location.dep_dir(&name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to remove '{}'", dir.display()))?;
        }
        report::note(format_args!("pruned '{name}' (no longer in the manifest)"));
    }
    if let Ok(entries) = std::fs::read_dir(location.deps_dir()) {
        for entry in entries.flatten() {
            let is_stale = entry
                .file_name()
                .to_str()
                .is_some_and(|name| !location.manifest.deps.contains_key(name));
            if is_stale {
                let path = entry.path();
                if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove '{}'", path.display()))?;
                } else {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("failed to remove '{}'", path.display()))?;
                }
                report::note(format_args!(
                    "pruned '{}' (no longer in the manifest)",
                    path.display()
                ));
            }
        }
    }

    if lock_changed {
        save_lock(&location, &lock)?;
    }
    Ok(())
}

/// `lait deps update [NAME...]`: re-resolve each selected dependency's ref
/// (every dep when NAME is omitted) and move its lock entry when the ref
/// now points at a different commit. A dep whose request drifted in the
/// manifest is updated to whatever the new request resolves to, same as
/// `install` without `--frozen` — the difference is that `update` always
/// re-resolves, even when the lock still covers the request.
pub(crate) async fn run_update(
    args: crate::cli::DepsUpdateArgs,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    crate::signal::spawn_handler(cancel.clone());
    let Some(location) = load_manifest_cancellable(cancel.clone()).await? else {
        bail!(
            "no {} found in this directory or its ancestors; add dependencies with `lait deps add` first",
            super::manifest::MANIFEST_FILE_NAME
        );
    };
    let selected: Vec<String> = if args.names.is_empty() {
        location.manifest.deps.keys().cloned().collect()
    } else {
        for name in &args.names {
            if !location.manifest.deps.contains_key(name) {
                bail!(
                    "no dependency named '{name}' in {}",
                    location.manifest_path().display()
                );
            }
        }
        args.names.clone()
    };
    if selected.is_empty() {
        println!(
            "no dependencies declared in {}",
            location.manifest_path().display()
        );
        return Ok(());
    }

    let mut lock = load_lock(&location)?;
    let client = GitHubClient::new();
    let mut lock_changed = false;
    for name in selected {
        let req = &location.manifest.deps[&name];
        let (spec, kind) = req.resolve(&name)?;
        let commit = client
            .resolve_commit(&spec.repo_slug(), spec.git_ref.as_deref(), &cancel)
            .await?;
        let needs_fetch = match lock.deps.get(&name) {
            Some(locked) if locked.commit == commit && locked.matches(&spec, kind) => {
                !matches!(payload_status(&location, locked), PayloadStatus::Installed)
            }
            _ => true,
        };
        if !needs_fetch {
            println!("{name}: already up to date ({})", short_commit(&commit));
            continue;
        }
        let content = client
            .fetch_file(&spec.repo_slug(), &commit, &spec.path, &cancel)
            .await?;
        validate_payload(kind, &name, &content)?;
        let file = write_payload(&location, &name, &spec, &content)?;
        let entry = LockedDep {
            kind,
            repo: spec.repo_slug(),
            path: spec.path.clone(),
            git_ref: spec.git_ref.clone(),
            commit: commit.clone(),
            sha256: LockFile::sha256_hex(&content),
            file,
        };
        lock.deps.insert(name.clone(), entry);
        lock_changed = true;
        println!("{name}: updated to {}", short_commit(&commit));
    }
    if lock_changed {
        save_lock(&location, &lock)?;
    }
    Ok(())
}

/// `lait deps remove <NAME>`: drop the dep from the manifest and lock and
/// delete its `.lait/deps/<name>/` directory. Purely local — the payload is
/// regenerable, so removal never needs the network.
pub(crate) fn run_remove(args: crate::cli::DepsNameArgs) -> Result<()> {
    let Some(mut location) = load_manifest()? else {
        bail!(
            "no {} found in this directory or its ancestors",
            super::manifest::MANIFEST_FILE_NAME
        );
    };
    if location.manifest.deps.remove(&args.name).is_none() {
        bail!(
            "no dependency named '{}' in {}",
            args.name,
            location.manifest_path().display()
        );
    }
    save_manifest(&location)?;

    let mut lock = load_lock(&location)?;
    if lock.deps.remove(&args.name).is_some() {
        save_lock(&location, &lock)?;
    }
    let dir = location.dep_dir(&args.name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to remove '{}'", dir.display()))?;
    }
    println!("removed {}", args.name);
    Ok(())
}

/// `lait deps list`: every manifest entry with its resolved request, the
/// locked commit, and the payload's on-disk status — the read-side
/// counterpart to `verify` (which hashes, this only reports).
pub(crate) fn run_list() -> Result<()> {
    let Some(location) = load_manifest()? else {
        println!(
            "no {} found in this directory or its ancestors",
            super::manifest::MANIFEST_FILE_NAME
        );
        return Ok(());
    };
    if location.manifest.deps.is_empty() {
        println!(
            "no dependencies declared in {}",
            location.manifest_path().display()
        );
        return Ok(());
    }
    let lock = load_lock(&location)?;
    for (name, req) in &location.manifest.deps {
        match req.resolve(name) {
            Err(error) => {
                println!("{name}  (invalid): {error:#}");
            }
            Ok((spec, kind)) => {
                let status = match lock.deps.get(name) {
                    None => "not locked".to_owned(),
                    Some(locked) if !locked.matches(&spec, kind) => {
                        "stale (manifest changed)".to_owned()
                    }
                    Some(locked) => match payload_status(&location, locked) {
                        PayloadStatus::Installed => {
                            format!("installed ({})", short_commit(&locked.commit))
                        }
                        PayloadStatus::Missing => "not installed".to_owned(),
                        PayloadStatus::Modified => "modified".to_owned(),
                    },
                };
                println!(
                    "{name}  {}  {}/{}@{}  {status}",
                    kind.name(),
                    spec.repo_slug(),
                    spec.path,
                    spec.git_ref.as_deref().unwrap_or("<default>"),
                );
            }
        }
    }
    Ok(())
}

/// `lait deps verify`: hash every locked dep's materialized file against
/// `lait.lock` — the integrity check for CI and for "did someone/something
/// edit `.lait/deps/`". Exits nonzero when anything declared is missing,
/// modified, or not covered by the lock.
pub(crate) fn run_verify() -> Result<()> {
    let Some(location) = load_manifest()? else {
        println!(
            "no {} found in this directory or its ancestors",
            super::manifest::MANIFEST_FILE_NAME
        );
        return Ok(());
    };
    let lock = load_lock(&location)?;
    let mut failures = 0usize;
    for (name, req) in &location.manifest.deps {
        let Ok((spec, kind)) = req.resolve(name) else {
            failures += 1;
            println!("{name}: FAILED (manifest entry does not parse)");
            continue;
        };
        match lock.deps.get(name) {
            None => {
                failures += 1;
                println!(
                    "{name}: FAILED (not covered by {})",
                    super::lock::LOCK_FILE_NAME
                );
            }
            Some(locked) if !locked.matches(&spec, kind) => {
                failures += 1;
                println!("{name}: FAILED (the manifest request changed; run `lait deps update`)");
            }
            Some(locked) => match payload_status(&location, locked) {
                PayloadStatus::Installed => {
                    println!("{name}: ok ({})", short_commit(&locked.commit));
                }
                PayloadStatus::Missing => {
                    failures += 1;
                    println!("{name}: FAILED (not installed; run `lait deps install`)");
                }
                PayloadStatus::Modified => {
                    failures += 1;
                    println!(
                        "{name}: FAILED ('{}' does not match the locked sha256)",
                        locked.absolute_file(&location).display()
                    );
                }
            },
        }
    }
    for name in lock.deps.keys() {
        if !location.manifest.deps.contains_key(name) {
            report::warn(format_args!(
                "{name}: locked but not declared in {} (stale lock entry)",
                super::manifest::MANIFEST_FILE_NAME
            ));
        }
    }
    if failures > 0 {
        bail!(
            "{failures} dependenc{} failed verification",
            if failures == 1 { "y" } else { "ies" }
        );
    }
    Ok(())
}

/// What a manifest contributes to the path registries — a name→path list
/// per kind, merged into `ConfigFile` by `config::load` (which owns those
/// maps). Each path is the dep's materialized location; whether the file
/// currently exists is deliberately not checked here — a dep not yet
/// installed should still resolve by name so the resulting "failed to read
/// '.lait/deps/...'" error (and `lait lint`'s registry check) points at the
/// real problem rather than looking like an unknown name.
#[derive(Debug, Default)]
pub(crate) struct RegistryEntries {
    pub(crate) workflows: Vec<(String, PathBuf)>,
    pub(crate) agents: Vec<(String, PathBuf)>,
    pub(crate) skills: Vec<(String, PathBuf)>,
}

/// `pub(super)` so `deps::tests` can check the name→path mapping directly
/// without going through a filesystem search for the manifest.
pub(super) fn entries_of(location: &ManifestLocation) -> Result<RegistryEntries> {
    let mut entries = RegistryEntries::default();
    for (name, req) in &location.manifest.deps {
        let (spec, kind) = req.resolve(name)?;
        let path = location.dir.join(payload_relpath(name, &spec)?);
        match kind {
            DepKind::Workflow => entries.workflows.push((name.clone(), path)),
            DepKind::Agent => entries.agents.push((name.clone(), path)),
            DepKind::Skill => entries.skills.push((name.clone(), path)),
        }
    }
    Ok(entries)
}

/// The config-merge entry point's sync half: find and read `lait.deps.yml`
/// from the current directory upward, then compute its registry entries.
/// `None`-manifest yields empty entries, so callers can merge
/// unconditionally.
pub(crate) fn load_registry_entries() -> Result<RegistryEntries> {
    match load_manifest()? {
        Some(location) => entries_of(&location),
        None => Ok(RegistryEntries::default()),
    }
}

/// The cancellation-aware counterpart for `load_config_cancellable` — the
/// manifest read runs on the bounded filesystem worker like the config's
/// own does.
pub(crate) async fn load_registry_entries_cancellable(
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<RegistryEntries> {
    match load_manifest_cancellable(cancellation).await? {
        Some(location) => entries_of(&location),
        None => Ok(RegistryEntries::default()),
    }
}
