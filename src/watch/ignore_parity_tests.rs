//! The watcher must skip exactly what the full walk skips, or ignored files
//! get re-embedded on every save (and stay indexed forever).

use super::*;
use crate::testing::EnvRestore;
use serial_test::serial;
use std::fs;
use tempfile::tempdir;

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn no_global_ignore(root: &Path) -> EnvRestore {
    let missing = root.join("no-global-ignore");
    EnvRestore::set(&[(
        crate::constants::GLOBAL_CODESEARCHIGNORE_ENV,
        missing.to_str().unwrap(),
    )])
}

#[test]
#[serial]
fn nested_ignore_files_exclude_paths_below_them() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let _env = no_global_ignore(root);
    write(&root.join("claude/.gitignore"), "sessions/\n");
    write(&root.join("docs/.codesearchignore"), "drafts/\n");
    for f in [
        "claude/sessions/1.rs",
        "docs/drafts/a.rs",
        "sessions/top.rs",
        "docs/final.rs",
    ] {
        write(&root.join(f), "fn x() {}\n");
    }

    let watcher = FileWatcher::new(root.to_path_buf());
    assert!(!watcher.is_watchable(&root.join("claude/sessions/1.rs")));
    assert!(!watcher.is_watchable(&root.join("docs/drafts/a.rs")));
    assert!(
        watcher.is_watchable(&root.join("sessions/top.rs")),
        "a nested ignore file only applies below its own directory"
    );
    assert!(watcher.is_watchable(&root.join("docs/final.rs")));
}

#[test]
#[serial]
fn a_nested_negation_overrides_a_root_rule() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let _env = no_global_ignore(root);
    write(&root.join(".gitignore"), "/claude/*\n!/claude/hooks/\n");
    write(&root.join("claude/hooks/.gitignore"), "*.rs\n!keep.rs\n");
    for f in [
        "claude/cache.rs",
        "claude/hooks/keep.rs",
        "claude/hooks/drop.rs",
    ] {
        write(&root.join(f), "fn x() {}\n");
    }

    let watcher = FileWatcher::new(root.to_path_buf());
    assert!(!watcher.is_watchable(&root.join("claude/cache.rs")));
    assert!(watcher.is_watchable(&root.join("claude/hooks/keep.rs")));
    assert!(!watcher.is_watchable(&root.join("claude/hooks/drop.rs")));
}

#[test]
#[serial]
fn hidden_paths_are_skipped_like_the_walker() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let _env = no_global_ignore(root);
    write(&root.join(".claude/settings.rs"), "fn x() {}\n");
    write(&root.join("src/.hidden.rs"), "fn x() {}\n");

    let watcher = FileWatcher::new(root.to_path_buf());
    assert!(!watcher.is_watchable(&root.join(".claude/settings.rs")));
    assert!(!watcher.is_watchable(&root.join("src/.hidden.rs")));
}

#[test]
#[serial]
fn an_edited_ignore_file_applies_after_its_change_event() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let _env = no_global_ignore(root);
    write(&root.join("gen/out.rs"), "fn x() {}\n");
    let watcher = FileWatcher::new(root.to_path_buf());
    assert!(watcher.is_watchable(&root.join("gen/out.rs")));

    write(&root.join("gen/.gitignore"), "*.rs\n");
    watcher.note_path_changed(&root.join("gen/.gitignore"));
    assert!(!watcher.is_watchable(&root.join("gen/out.rs")));

    write(&root.join(".gitignore"), "gen/\n");
    fs::remove_file(root.join("gen/.gitignore")).unwrap();
    watcher.note_path_changed(&root.join("gen/.gitignore"));
    watcher.note_path_changed(&root.join(".gitignore"));
    assert!(
        !watcher.is_watchable(&root.join("gen/out.rs")),
        "root rule reloaded"
    );
}

#[test]
#[serial]
fn the_global_ignore_file_applies_to_watched_paths() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    let global = dir.path().join("global-ignore");
    write(&global, "**/skills/synced/\n");
    let _env = EnvRestore::set(&[(
        crate::constants::GLOBAL_CODESEARCHIGNORE_ENV,
        global.to_str().unwrap(),
    )]);
    write(&root.join("claude/skills/synced/m.rs"), "fn x() {}\n");

    let watcher = FileWatcher::new(root.clone());
    assert!(!watcher.is_watchable(&root.join("claude/skills/synced/m.rs")));
}

#[cfg(target_os = "macos")]
#[test]
#[serial]
fn canonical_event_paths_still_match_a_symlinked_root() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    assert!(
        root.starts_with("/var"),
        "macOS tempdirs live under the /var symlink"
    );
    let _env = no_global_ignore(root);
    write(&root.join(".gitignore"), "build/\n");
    write(&root.join("build/out.rs"), "fn x() {}\n");

    let watcher = FileWatcher::new(root.to_path_buf());
    let canonical = crate::cache::safe_canonicalize(&root.join("build/out.rs")).unwrap();
    assert!(canonical.starts_with("/private/var"));
    assert!(
        !watcher.is_watchable(&canonical),
        "FSEvents reports /private/var paths; root-anchored rules must still apply"
    );
}
