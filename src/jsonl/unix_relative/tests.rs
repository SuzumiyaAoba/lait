use std::{
    os::unix::fs::symlink,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{append, directory_exists, open, path_exists, read_dir, remove};

/// A uniquely named relative directory under the crate root — the unit test
/// binary's CWD — since every function in this module rejects an absolute
/// path outright (see `relative_names`). Using `std::env::set_current_dir`
/// to get a hermetic CWD instead was rejected: it is process-global mutable
/// state, and these tests run as threads inside the same binary as every
/// other unit test, many of which touch the filesystem through relative or
/// absolute paths of their own — a `set_current_dir` here would corrupt any
/// of them intermittently. A uniquely named fixture directory needs no such
/// global mutation and is safe under `cargo test`'s parallel harness.
struct RelativeFixture(PathBuf);

impl RelativeFixture {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after the unix epoch")
            .as_nanos();
        Self(PathBuf::from(format!(
            ".lait-unix-relative-test-{label}-{}-{nanos}-{counter}",
            std::process::id()
        )))
    }

    fn join(&self, sub: &str) -> PathBuf {
        self.0.join(sub)
    }
}

impl Drop for RelativeFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn append_creates_missing_parent_directories_and_records_are_readable_back() {
    let fixture = RelativeFixture::new("append-create");
    let log = fixture.join("a/b/log.jsonl");

    append(&log, ["one", "two"]).expect("append should create missing parents");

    let contents = std::fs::read_to_string(&log).expect("log file should exist");
    assert_eq!(contents, "\"one\"\n\"two\"\n");
}

#[test]
fn path_exists_and_directory_exists_report_absence_without_creating_anything() {
    let fixture = RelativeFixture::new("existing-intent");
    let missing = fixture.join("does/not/exist.jsonl");

    assert!(!path_exists(&missing).expect("path_exists should not error on a missing ancestor"));
    assert!(
        !directory_exists(&fixture.join("does/not"))
            .expect("directory_exists should not error on a missing ancestor")
    );
    // `OpenIntent::Existing` (used internally by both calls above) must
    // never create the missing intermediate directories it walks through
    // while just checking for existence — unlike `append`'s
    // `OpenIntent::Create`.
    assert!(
        !fixture.0.exists(),
        "checking existence must not create the fixture directory"
    );
}

#[test]
fn remove_fails_clearly_when_the_file_does_not_exist() {
    let fixture = RelativeFixture::new("remove-missing");
    std::fs::create_dir_all(&fixture.0).unwrap();

    let error = remove(&fixture.join("missing.jsonl")).unwrap_err();
    assert!(
        format!("{error:#}").contains("not found") || format!("{error:#}").contains("no such"),
        "error: {error:#}"
    );
}

#[test]
fn non_final_component_symlink_is_rejected() {
    let fixture = RelativeFixture::new("symlink-nonfinal");
    let real_target = fixture.join("real");
    std::fs::create_dir_all(&real_target).unwrap();
    let link = fixture.join("link");
    symlink(std::fs::canonicalize(&real_target).unwrap(), &link).unwrap();

    let error = append(&link.join("file.jsonl"), ["value"]).unwrap_err();
    assert!(
        format!("{error:#}").contains("symbolic link"),
        "error: {error:#}"
    );
}

#[test]
fn final_component_symlink_is_rejected_by_open_and_path_exists() {
    let fixture = RelativeFixture::new("symlink-final");
    std::fs::create_dir_all(&fixture.0).unwrap();
    let target = fixture.join("target.jsonl");
    std::fs::write(&target, "\"real\"\n").unwrap();
    let link = fixture.join("link.jsonl");
    symlink(std::fs::canonicalize(&target).unwrap(), &link).unwrap();

    let open_error = open(&link).unwrap_err();
    assert!(
        format!("{open_error:#}").contains("symbolic link"),
        "error: {open_error:#}"
    );

    let path_exists_error = path_exists(&link).unwrap_err();
    assert!(
        format!("{path_exists_error:#}").contains("symbolic link"),
        "error: {path_exists_error:#}"
    );
}

#[test]
fn relative_names_rejects_parent_dir_absolute_and_nul_components() {
    let fixture = RelativeFixture::new("bad-components");

    let absolute_error = append(&PathBuf::from("/tmp/should-not-be-absolute"), ["x"]).unwrap_err();
    assert!(
        format!("{absolute_error:#}").contains("must not be absolute"),
        "error: {absolute_error:#}"
    );

    let parent_dir_error = append(&fixture.join("..").join("escape.jsonl"), ["x"]).unwrap_err();
    assert!(
        format!("{parent_dir_error:#}").contains("parent"),
        "error: {parent_dir_error:#}"
    );

    let nul_path = fixture.join("bad\0name.jsonl");
    let nul_error = append(&nul_path, ["x"]).unwrap_err();
    assert!(
        format!("{nul_error:#}").contains("NUL byte"),
        "error: {nul_error:#}"
    );
}

#[test]
fn read_dir_skips_dot_and_dotdot_and_flags_symlinks() {
    let fixture = RelativeFixture::new("read-dir");
    std::fs::create_dir_all(&fixture.0).unwrap();
    std::fs::write(fixture.join("regular.jsonl"), "\"x\"\n").unwrap();
    let target = fixture.join("regular.jsonl");
    symlink(
        std::fs::canonicalize(&target).unwrap(),
        fixture.join("link.jsonl"),
    )
    .unwrap();

    let mut entries = read_dir(&fixture.0).expect("read_dir should list the fixture directory");
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let names: Vec<String> = entries
        .iter()
        .map(|entry| entry.name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["link.jsonl", "regular.jsonl"]);
    for entry in &entries {
        let name = entry.name.to_string_lossy();
        let expected_symlink = name == "link.jsonl";
        assert_eq!(
            entry.is_symlink, expected_symlink,
            "entry {name:?} should have is_symlink = {expected_symlink}",
        );
    }
}
