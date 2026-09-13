use std::{os::unix::fs::symlink, path::Path};

use super::{append, directory_exists, open, path_exists, read_dir, remove};
use crate::test_support::in_temp_dir;

// Every function in this module rejects an absolute path outright (see
// `relative_names`), so a fixture has to be a real relative path resolved
// against the current directory. `test_support::in_temp_dir` (not a
// module-local fixture directory under the repo's own working tree) is the
// crate's one established way to do that safely: it holds a process-wide
// lock for its whole body and swaps the current directory to a fresh,
// unique temp directory, so these tests cannot race with each other or with
// any other `#[cfg(test)]` module's own relative-path tests running
// concurrently in the same `cargo test` binary.

#[test]
fn append_creates_missing_parent_directories_and_records_are_readable_back() {
    in_temp_dir("jsonl-unix-relative-append-create", || {
        let log = Path::new("a/b/log.jsonl");
        append(log, ["one", "two"]).expect("append should create missing parents");

        let contents = std::fs::read_to_string(log).expect("log file should exist");
        assert_eq!(contents, "\"one\"\n\"two\"\n");
    });
}

#[test]
fn path_exists_and_directory_exists_report_absence_without_creating_anything() {
    in_temp_dir("jsonl-unix-relative-existing-intent", || {
        let missing = Path::new("does/not/exist.jsonl");

        assert!(!path_exists(missing).expect("path_exists should not error on a missing ancestor"));
        assert!(
            !directory_exists(Path::new("does/not"))
                .expect("directory_exists should not error on a missing ancestor")
        );
        // `OpenIntent::Existing` (used internally by both calls above) must
        // never create the missing intermediate directories it walks
        // through while just checking existence — unlike `append`'s
        // `OpenIntent::Create`.
        assert!(
            !Path::new("does").exists(),
            "checking existence must not create the missing directory"
        );
    });
}

#[test]
fn remove_fails_clearly_when_the_file_does_not_exist() {
    in_temp_dir("jsonl-unix-relative-remove-missing", || {
        let error = remove(Path::new("missing.jsonl")).unwrap_err();
        assert!(
            format!("{error:#}").contains("not found"),
            "error: {error:#}"
        );
    });
}

#[test]
fn non_final_component_symlink_is_rejected() {
    in_temp_dir("jsonl-unix-relative-symlink-nonfinal", || {
        std::fs::create_dir_all("real").unwrap();
        symlink(std::fs::canonicalize("real").unwrap(), "link").unwrap();

        let error = append(Path::new("link/file.jsonl"), ["value"]).unwrap_err();
        assert!(
            format!("{error:#}").contains("symbolic link"),
            "error: {error:#}"
        );
    });
}

#[test]
fn final_component_symlink_is_rejected_by_open_and_path_exists() {
    in_temp_dir("jsonl-unix-relative-symlink-final", || {
        std::fs::write("target.jsonl", "\"real\"\n").unwrap();
        symlink(std::fs::canonicalize("target.jsonl").unwrap(), "link.jsonl").unwrap();

        let open_error = open(Path::new("link.jsonl")).unwrap_err();
        assert!(
            format!("{open_error:#}").contains("symbolic link"),
            "error: {open_error:#}"
        );

        let path_exists_error = path_exists(Path::new("link.jsonl")).unwrap_err();
        assert!(
            format!("{path_exists_error:#}").contains("symbolic link"),
            "error: {path_exists_error:#}"
        );
    });
}

#[test]
fn relative_names_rejects_parent_dir_absolute_and_nul_components() {
    in_temp_dir("jsonl-unix-relative-bad-components", || {
        let absolute_error = append(Path::new("/tmp/should-not-be-absolute"), ["x"]).unwrap_err();
        assert!(
            format!("{absolute_error:#}").contains("must not be absolute"),
            "error: {absolute_error:#}"
        );

        let parent_dir_error = append(Path::new("../escape.jsonl"), ["x"]).unwrap_err();
        assert!(
            format!("{parent_dir_error:#}").contains("parent"),
            "error: {parent_dir_error:#}"
        );

        let nul_error = append(Path::new("bad\0name.jsonl"), ["x"]).unwrap_err();
        assert!(
            format!("{nul_error:#}").contains("NUL byte"),
            "error: {nul_error:#}"
        );
    });
}

#[test]
fn read_dir_skips_dot_and_dotdot_and_flags_symlinks() {
    in_temp_dir("jsonl-unix-relative-read-dir", || {
        std::fs::write("regular.jsonl", "\"x\"\n").unwrap();
        symlink(
            std::fs::canonicalize("regular.jsonl").unwrap(),
            "link.jsonl",
        )
        .unwrap();

        let mut entries = read_dir(Path::new(".")).expect("read_dir should list the temp dir");
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
    });
}
