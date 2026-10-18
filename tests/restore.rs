use airbag::restore::{self, Change, Report, Status};
use airbag::snapshot::{Node, Settings, find_node, take};
use airbag::store::{Action, Kind, Project, Store, now_ms};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    File(u32, Vec<u8>),
    Dir(u32),
    Link(String),
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    store: Store,
    project: Project,
    settings: Settings,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    write(&root, ".git/HEAD", "ref: refs/heads/main\n");
    write(
        &root,
        ".git/refs/heads/main",
        "1111111111111111111111111111111111111111\n",
    );
    write(&root, "src/main.rs", "fn main() {}\n");
    write(&root, "src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    write(&root, ".env", "API_KEY=secret\n");
    write(&root, "run.sh", "#!/bin/sh\necho hi\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("src/main.rs", root.join("entry")).unwrap();
    fs::create_dir_all(root.join("assets/empty")).unwrap();
    let store = Store::at(temp.path().join("store")).unwrap();
    let project = store.project(&root).unwrap();
    Fixture {
        root: project.root.clone(),
        _temp: temp,
        store,
        project,
        settings: Settings::default(),
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

fn state(root: &Path) -> BTreeMap<String, Entry> {
    let mut entries = BTreeMap::new();
    collect(root, "", &mut entries);
    entries
}

fn collect(dir: &Path, rel: &str, entries: &mut BTreeMap<String, Entry>) {
    for item in fs::read_dir(dir).unwrap() {
        let item = item.unwrap();
        let name = item.file_name().into_string().unwrap();
        if name == "node_modules" {
            continue;
        }
        let path = item.path();
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let meta = fs::symlink_metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o7777;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path).unwrap();
            entries.insert(child, Entry::Link(target.to_str().unwrap().into()));
        } else if meta.is_dir() {
            entries.insert(child.clone(), Entry::Dir(mode));
            collect(&path, &child, entries);
        } else {
            entries.insert(child, Entry::File(mode, fs::read(&path).unwrap()));
        }
    }
}

fn agent(f: &Fixture, detail: &str) -> u64 {
    let action = Action {
        agent: "claude".into(),
        tool: "Bash".into(),
        detail: detail.into(),
        session: None,
        at_ms: now_ms(),
    };
    take(&f.store, &f.project, &f.settings, Kind::Agent, Some(action))
        .unwrap()
        .id
}

fn undo(f: &Fixture, scope: Option<&str>) -> Report {
    let current = restore::current_tree(&f.store, &f.project, &f.settings).unwrap();
    let target = restore::undo_target(&f.store, &f.project, &current, scope)
        .unwrap()
        .expect("there should be something to undo");
    restore::restore(&f.store, &f.project, &f.settings, &target, scope)
        .unwrap()
        .report
}

fn wreck(root: &Path) {
    fs::remove_dir_all(root.join("src")).unwrap();
    fs::remove_dir_all(root.join(".git")).unwrap();
    fs::remove_dir_all(root.join("assets")).unwrap();
    write(root, ".env", "API_KEY=leaked\n");
    write(root, "new/deep/file.txt", "created by the agent\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(root.join("entry")).unwrap();
    symlink("elsewhere", root.join("entry")).unwrap();
}

#[test]
fn undo_restores_everything_an_agent_wrecked() {
    let f = fixture();
    let before = state(&f.root);
    agent(&f, "rm -rf src .git assets && echo leaked > .env");
    wreck(&f.root);

    let report = undo(&f, None);

    assert_eq!(state(&f.root), before);
    assert_eq!(report.written, 7);
    assert_eq!(report.removed, 1);
    assert!(report.kept.is_empty(), "{:?}", report.kept);
}

#[test]
fn redo_reverts_the_undo() {
    let f = fixture();
    agent(&f, "rm -rf src");
    wreck(&f.root);
    let wrecked = state(&f.root);
    undo(&f, None);

    let target = restore::redo_target(&f.project)
        .unwrap()
        .expect("an undo to revert");
    restore::restore(&f.store, &f.project, &f.settings, &target, None).unwrap();

    assert_eq!(state(&f.root), wrecked);
}

#[test]
fn undo_steps_back_one_action_at_a_time() {
    let f = fixture();
    write(&f.root, "notes.txt", "v1");
    agent(&f, "echo v2 > notes.txt");
    write(&f.root, "notes.txt", "v2");
    agent(&f, "echo v3 > notes.txt");
    write(&f.root, "notes.txt", "v3");

    undo(&f, None);
    assert_eq!(read(&f.root, "notes.txt"), "v2");
    undo(&f, None);
    assert_eq!(read(&f.root, "notes.txt"), "v1");
}

#[test]
fn folders_airbag_does_not_track_survive_an_undo() {
    let f = fixture();
    write(&f.root, "node_modules/react/index.js", "react");
    agent(&f, "rm -rf src && npm install new-pkg");
    fs::remove_dir_all(f.root.join("src")).unwrap();
    write(&f.root, "pkg/node_modules/dep/index.js", "dep");
    write(&f.root, "pkg/index.js", "tracked");

    let report = undo(&f, None);

    assert_eq!(read(&f.root, "node_modules/react/index.js"), "react");
    assert_eq!(read(&f.root, "src/main.rs"), "fn main() {}\n");
    assert!(!f.root.join("pkg/index.js").exists());
    assert_eq!(read(&f.root, "pkg/node_modules/dep/index.js"), "dep");
    assert!(report.kept.iter().any(|note| note.starts_with("pkg/")));
}

#[test]
fn untracked_file_in_the_way_is_moved_aside_not_overwritten() {
    let mut f = fixture();
    f.settings = Settings {
        reflink: false,
        max_file_bytes: 64,
        ..Settings::default()
    };
    write(&f.root, "data.bin", "small original");
    agent(&f, "generate data");
    write(&f.root, "data.bin", &"x".repeat(1000));

    let report = undo(&f, None);

    assert_eq!(read(&f.root, "data.bin"), "small original");
    let aside: Vec<String> = fs::read_dir(&f.root)
        .unwrap()
        .map(|item| item.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("data.bin.airbag-kept-"))
        .collect();
    assert_eq!(aside.len(), 1);
    assert_eq!(fs::read(f.root.join(&aside[0])).unwrap().len(), 1000);
    assert!(report.kept.iter().any(|note| note.starts_with("data.bin")));
}

#[test]
fn scoped_undo_only_touches_that_path() {
    let f = fixture();
    agent(&f, "edit two files");
    write(&f.root, ".env", "API_KEY=leaked\n");
    write(&f.root, "src/main.rs", "fn main() { todo!() }\n");

    undo(&f, Some(".env"));

    assert_eq!(read(&f.root, ".env"), "API_KEY=secret\n");
    assert_eq!(read(&f.root, "src/main.rs"), "fn main() { todo!() }\n");
}

#[test]
fn scoped_undo_brings_back_a_deleted_folder() {
    let f = fixture();
    agent(&f, "rm -rf src");
    fs::remove_dir_all(f.root.join("src")).unwrap();
    write(&f.root, ".env", "changed\n");

    undo(&f, Some("src"));

    assert_eq!(
        read(&f.root, "src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n"
    );
    assert_eq!(read(&f.root, ".env"), "changed\n");
}

#[test]
fn diff_lists_what_changed_since_the_snapshot() {
    let f = fixture();
    agent(&f, "shuffle files");
    fs::remove_file(f.root.join("src/lib.rs")).unwrap();
    write(&f.root, "src/main.rs", "fn main() { println!(\"hi\") }\n");
    write(&f.root, "docs/guide.md", "# guide\n");

    let current = restore::current_tree(&f.store, &f.project, &f.settings).unwrap();
    let target = restore::undo_target(&f.store, &f.project, &current, None)
        .unwrap()
        .unwrap();
    let changes = restore::diff(&f.store, &target.tree, &current, None).unwrap();

    let change = |status, path: &str| Change {
        status,
        path: path.into(),
    };
    assert_eq!(
        changes,
        vec![
            change(Status::Added, "docs/guide.md"),
            change(Status::Deleted, "src/lib.rs"),
            change(Status::Modified, "src/main.rs"),
        ]
    );
}

#[test]
fn read_only_folders_come_back_with_their_mode() {
    let f = fixture();
    write(&f.root, "locked/file.txt", "keep me");
    fs::set_permissions(f.root.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
    let before = state(&f.root);
    agent(&f, "chmod u+w locked && rm locked/file.txt");
    fs::set_permissions(f.root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(f.root.join("locked/file.txt")).unwrap();

    undo(&f, None);
    let after = state(&f.root);
    fs::set_permissions(f.root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(after, before);
}

#[test]
fn missing_stored_copy_aborts_before_touching_anything() {
    let f = fixture();
    agent(&f, "rm -rf src");
    let snapshot = f.project.snapshots().unwrap().pop().unwrap();
    let Some(Node::File { hash, .. }) = find_node(&f.store, &snapshot.tree, "src/lib.rs").unwrap()
    else {
        panic!("src/lib.rs is missing from the snapshot");
    };
    fs::remove_file(f.store.object_path(&hash)).unwrap();
    fs::remove_dir_all(f.root.join("src")).unwrap();
    let wrecked = state(&f.root);

    let error = restore::restore(&f.store, &f.project, &f.settings, &snapshot, None)
        .err()
        .expect("restore should refuse");

    assert!(format!("{error:#}").contains("missing"), "{error:#}");
    assert_eq!(state(&f.root), wrecked);
    assert_eq!(f.project.ids().unwrap(), [snapshot.id]);
}

#[test]
fn scoped_undo_never_follows_a_folder_swapped_for_a_link() {
    let f = fixture();
    write(&f.root, "a/b/c.txt", "secret\n");
    agent(&f, "swap a for a link");
    let outside = f._temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::remove_dir_all(f.root.join("a")).unwrap();
    symlink(&outside, f.root.join("a")).unwrap();
    let current = restore::current_tree(&f.store, &f.project, &f.settings).unwrap();
    let target = restore::undo_target(&f.store, &f.project, &current, Some("a/b/c.txt"))
        .unwrap()
        .unwrap();

    let error = restore::restore(
        &f.store,
        &f.project,
        &f.settings,
        &target,
        Some("a/b/c.txt"),
    )
    .err()
    .expect("a scoped undo through a link must refuse");

    assert!(
        format!("{error:#}").contains("no longer a folder"),
        "{error:#}"
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert!(
        fs::symlink_metadata(f.root.join("a"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    undo(&f, None);
    assert_eq!(read(&f.root, "a/b/c.txt"), "secret\n");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn undo_works_in_a_read_only_root_and_keeps_its_mode() {
    let f = fixture();
    agent(&f, "touch junk.txt");
    write(&f.root, "junk.txt", "junk");
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o555)).unwrap();

    undo(&f, None);
    let mode = fs::metadata(&f.root).unwrap().permissions().mode() & 0o7777;
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!f.root.join("junk.txt").exists());
    assert_eq!(mode, 0o555);
}
