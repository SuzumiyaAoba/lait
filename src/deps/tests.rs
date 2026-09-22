//! Unit tests for the deps module's pure parts: source-spec parsing,
//! name/kind derivation, manifest and lock file round-trips, and the
//! manifest→registry-entries computation. Anything needing the filesystem
//! layout of a real project (or a mock GitHub server) lives in
//! `tests/deps.rs` instead — see `tests/support` for the harness pattern.

use std::path::Path;

use super::{
    lock::{LOCK_FILE_NAME, LockFile, LockedDep, load_lock, save_lock},
    manifest::{
        DepRequirement, DepsManifest, MANIFEST_FILE_NAME, find_manifest_upward, save_manifest,
    },
    spec::{self, DepKind},
};

// ----- spec parsing -------------------------------------------------------

#[test]
fn parses_the_bare_shorthand() {
    let spec = spec::parse("owner/repo/workflows/review.yml").unwrap();
    assert_eq!(spec.owner, "owner");
    assert_eq!(spec.repo, "repo");
    assert_eq!(spec.path, "workflows/review.yml");
    assert_eq!(spec.git_ref, None);
    assert_eq!(spec.repo_slug(), "owner/repo");
    assert_eq!(spec.to_source(), "github:owner/repo/workflows/review.yml");
}

#[test]
fn parses_the_github_prefixed_form_and_an_at_ref() {
    let spec = spec::parse("github:owner/repo/a/b.yml@v1.2.3").unwrap();
    assert_eq!(spec.path, "a/b.yml");
    assert_eq!(spec.git_ref.as_deref(), Some("v1.2.3"));
}

#[test]
fn at_ref_keeps_slashes_in_branch_names() {
    let spec = spec::parse("owner/repo/f.yml@feature/slashes").unwrap();
    assert_eq!(spec.git_ref.as_deref(), Some("feature/slashes"));
}

#[test]
fn parses_a_github_blob_url() {
    let spec = spec::parse("https://github.com/owner/repo/blob/main/workflows/review.yml").unwrap();
    assert_eq!((spec.owner.as_str(), spec.repo.as_str()), ("owner", "repo"));
    assert_eq!(spec.path, "workflows/review.yml");
    assert_eq!(spec.git_ref.as_deref(), Some("main"));
}

#[test]
fn parses_a_raw_githubusercontent_url() {
    let spec =
        spec::parse("https://raw.githubusercontent.com/owner/repo/v2/agents/helper.md").unwrap();
    assert_eq!(spec.path, "agents/helper.md");
    assert_eq!(spec.git_ref.as_deref(), Some("v2"));
}

#[test]
fn strips_a_git_suffix_from_the_repo_name() {
    let spec = spec::parse("owner/repo.git/f.yml").unwrap();
    assert_eq!(spec.repo, "repo");
}

#[test]
fn rejects_specs_without_a_path_or_with_bad_segments() {
    for source in [
        "",
        "owner",
        "owner/repo",
        "github:owner/repo",
        "owner/repo/../secret.yml",
        "owner/repo/dir//file.yml",
        "owner/repo/.",
        "o%wner/repo/f.yml",
        "owner/repo/f.yml@",
        "owner/repo/f.yml@bad..ref",
        "https://github.com/owner/repo",
        "https://github.com/owner/repo/issues/1",
    ] {
        assert!(
            spec::parse(source).is_err(),
            "spec '{source}' should be rejected"
        );
    }
}

// ----- names and kinds ----------------------------------------------------

#[test]
fn derives_names_from_the_basename_or_skill_directory() {
    assert_eq!(spec::derive_name("workflows/review.yml").unwrap(), "review");
    assert_eq!(spec::derive_name("agents/helper.md").unwrap(), "helper");
    // A SKILL.md names its parent directory, not "SKILL".
    assert_eq!(
        spec::derive_name("skills/code-review/SKILL.md").unwrap(),
        "code-review"
    );
    assert_eq!(spec::derive_name("skills/SKILL.md").unwrap(), "skills");
}

#[test]
fn infers_kinds_from_the_path_extension() {
    assert_eq!(DepKind::infer("a/b.yml"), Some(DepKind::Workflow));
    assert_eq!(DepKind::infer("a/b.yaml"), Some(DepKind::Workflow));
    assert_eq!(DepKind::infer("a/b.md"), Some(DepKind::Agent));
    assert_eq!(DepKind::infer("a/b/SKILL.md"), Some(DepKind::Skill));
    assert_eq!(DepKind::infer("a/b/skill.md"), Some(DepKind::Skill));
    assert_eq!(DepKind::infer("a/b.txt"), None);
}

#[test]
fn rejects_names_outside_the_registry_charset() {
    for name in ["", "has space", "a/b", "..", "a.b", "日本語"] {
        assert!(spec::validate_name(name).is_err(), "name '{name}'");
    }
    for name in ["a", "a-b_c-1", "REVIEW"] {
        spec::validate_name(name).unwrap();
    }
}

// ----- manifest -----------------------------------------------------------

#[test]
fn manifest_round_trips_stably() {
    let yaml = "version: 1\ndeps:\n  b-dep:\n    source: github:o/r/b.yml\n  a-dep:\n    source: github:o/r/a.md\n    ref: main\n    kind: agent\n";
    let manifest = DepsManifest::parse(Path::new(MANIFEST_FILE_NAME), yaml).unwrap();
    assert_eq!(manifest.deps.len(), 2);
    // BTreeMap ordering keeps the serialization sorted regardless of the
    // input order — the file is meant to be diffed.
    let serialized = serde_yaml::to_string(&manifest).unwrap();
    let a_pos = serialized.find("a-dep").unwrap();
    let b_pos = serialized.find("b-dep").unwrap();
    assert!(
        a_pos < b_pos,
        "serialized order should be sorted: {serialized}"
    );
}

#[test]
fn manifest_rejects_a_newer_version_and_bad_names() {
    for yaml in [
        "version: 99\ndeps: {}\n",
        "deps:\n  'bad name':\n    source: github:o/r/f.yml\n",
        "deps:\n  ok:\n    source: github:o/r/f.yml\nextra: true\n",
    ] {
        assert!(
            DepsManifest::parse(Path::new(MANIFEST_FILE_NAME), yaml).is_err(),
            "manifest should be rejected: {yaml}"
        );
    }
}

#[test]
fn requirement_resolve_overlays_ref_and_kind() {
    let req = DepRequirement {
        source: "owner/repo/f.yml@main".to_owned(),
        git_ref: Some("v1".to_owned()),
        kind: None,
    };
    let (spec, kind) = req.resolve("x").unwrap();
    // The explicit `ref:` field wins over the `@` inside `source`.
    assert_eq!(spec.git_ref.as_deref(), Some("v1"));
    assert_eq!(kind, DepKind::Workflow);

    let req = DepRequirement {
        source: "github:owner/repo/f.txt".to_owned(),
        git_ref: None,
        kind: Some(DepKind::Skill),
    };
    // An explicit `kind` beats extension inference (which would fail on
    // `.txt` entirely).
    assert_eq!(req.resolve("x").unwrap().1, DepKind::Skill);
}

#[test]
fn find_manifest_upward_walks_ancestors() {
    let root = crate::test_support::TempDir::new("lait-deps-manifest");
    std::fs::write(
        root.path().join(MANIFEST_FILE_NAME),
        "version: 1\ndeps: {}\n",
    )
    .unwrap();
    let nested = root.path().join("a").join("b");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(
        find_manifest_upward(&nested).as_deref(),
        Some(root.path().join(MANIFEST_FILE_NAME).as_path())
    );
}

// ----- lock ---------------------------------------------------------------

#[test]
fn locked_dep_matches_only_on_full_request_identity() {
    let locked = LockedDep {
        kind: DepKind::Workflow,
        repo: "o/r".to_owned(),
        path: "f.yml".to_owned(),
        git_ref: Some("main".to_owned()),
        commit: "abc123".to_owned(),
        sha256: "0".repeat(64),
        file: ".lait/deps/x/f.yml".to_owned(),
    };
    let spec = spec::parse("o/r/f.yml@main").unwrap();
    assert!(locked.matches(&spec, DepKind::Workflow));
    assert!(!locked.matches(&spec, DepKind::Agent));
    assert!(!locked.matches(&spec::parse("o/r/f.yml").unwrap(), DepKind::Workflow));
    assert!(!locked.matches(&spec::parse("o/r/g.yml@main").unwrap(), DepKind::Workflow));
    assert!(!locked.matches(&spec::parse("o/s/f.yml@main").unwrap(), DepKind::Workflow));
}

#[test]
fn lock_round_trip_and_missing_file_is_empty() {
    let root = crate::test_support::TempDir::new("lait-deps-lock");
    let location = super::manifest::ManifestLocation {
        dir: root.path().to_path_buf(),
        manifest: DepsManifest::default(),
    };
    // Absent is not an error: an empty lock.
    let lock = load_lock(&location).unwrap();
    assert!(lock.deps.is_empty());

    let mut lock = LockFile::default();
    lock.deps.insert(
        "dep".to_owned(),
        LockedDep {
            kind: DepKind::Agent,
            repo: "o/r".to_owned(),
            path: "a.md".to_owned(),
            git_ref: None,
            commit: "0123456789abcdef".repeat(2).chars().take(40).collect(),
            sha256: LockFile::sha256_hex(b"payload"),
            file: ".lait/deps/dep/a.md".to_owned(),
        },
    );
    save_lock(&location, &lock).unwrap();
    let reloaded = load_lock(&location).unwrap();
    let entry = &reloaded.deps["dep"];
    assert_eq!(entry.kind, DepKind::Agent);
    assert_eq!(entry.sha256, LockFile::sha256_hex(b"payload"));
    assert_eq!(
        entry.absolute_file(&location),
        root.path().join(".lait/deps/dep/a.md")
    );
}

#[test]
fn lock_rejects_a_newer_version() {
    let root = crate::test_support::TempDir::new("lait-deps-lock-version");
    std::fs::write(root.path().join(LOCK_FILE_NAME), "version: 99\ndeps: {}\n").unwrap();
    let location = super::manifest::ManifestLocation {
        dir: root.path().to_path_buf(),
        manifest: DepsManifest::default(),
    };
    assert!(load_lock(&location).is_err());
}

// ----- registry entries ---------------------------------------------------

#[test]
fn registry_entries_map_deps_to_their_materialized_paths() {
    let root = crate::test_support::TempDir::new("lait-deps-entries");
    let manifest = DepsManifest::parse(
        Path::new(MANIFEST_FILE_NAME),
        "version: 1\ndeps:\n  review:\n    source: github:o/r/workflows/review.yml\n  helper:\n    source: github:o/r/agents/helper.md\n  style:\n    source: github:o/r/skills/style/SKILL.md\n",
    )
    .unwrap();
    let location = super::manifest::ManifestLocation {
        dir: root.path().to_path_buf(),
        manifest,
    };
    let entries = super::ops::entries_of(&location).unwrap();
    assert_eq!(
        entries.workflows.as_slice(),
        &[(
            "review".to_owned(),
            root.path().join(".lait/deps/review/review.yml")
        )]
    );
    assert_eq!(
        entries.agents.as_slice(),
        &[(
            "helper".to_owned(),
            root.path().join(".lait/deps/helper/helper.md")
        )]
    );
    assert_eq!(
        entries.skills.as_slice(),
        &[(
            "style".to_owned(),
            root.path().join(".lait/deps/style/SKILL.md")
        )]
    );
}

#[test]
fn save_and_load_manifest_round_trip() {
    let root = crate::test_support::TempDir::new("lait-deps-manifest-io");
    let mut manifest = DepsManifest::default();
    manifest.deps.insert(
        "dep".to_owned(),
        DepRequirement {
            source: "github:o/r/f.yml".to_owned(),
            git_ref: None,
            kind: None,
        },
    );
    let location = super::manifest::ManifestLocation {
        dir: root.path().to_path_buf(),
        manifest,
    };
    save_manifest(&location).unwrap();
    assert!(location.manifest_path().is_file());
}
