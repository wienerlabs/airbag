use airbag::snapshot::{Node, Settings, find_node, find_root, take};
use airbag::store::{Action, Kind, Store, now_ms};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    store: Store,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    write(&root, ".git/HEAD", "ref: refs/heads/main\n");
    let store = Store::at(temp.path().join("store")).unwrap();
    Fixture {
        _temp: temp,
        root,
        store,
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn set_mtime(path: &Path, time: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

fn action(detail: &str) -> Option<Action> {
    Some(Action {
        agent: "test".into(),
        tool: "Bash".into(),
        detail: detail.into(),
        session: None,
        at_ms: now_ms(),
    })
}

#[test]
fn captures_files_links_and_git_but_not_excluded_folders() {
    let f = fixture();
    write(&f.root, "src/main.rs", "fn main() {}\n");
    write(&f.root, ".env", "SECRET=1\n");
    write(
        &f.root,
        "node_modules/left-pad/index.js",
        "module.exports = 1\n",
    );
    fs::set_permissions(
        f.root.join("src/main.rs"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("src/main.rs", f.root.join("entry")).unwrap();
    let project = f.store.project(&f.root).unwrap();

    let snap = take(&f.store, &project, &Settings::default(), Kind::Manual, None).unwrap();

    let Some(Node::File {
        mode, size, hash, ..
    }) = find_node(&f.store, &snap.tree, "src/main.rs").unwrap()
    else {
        panic!("src/main.rs is missing from the snapshot");
    };
    assert_eq!((mode, size), (0o755, 13));
    assert_eq!(
        fs::read(f.store.object_path(&hash)).unwrap(),
        b"fn main() {}\n"
    );
    assert!(matches!(
        find_node(&f.store, &snap.tree, ".env").unwrap(),
        Some(Node::File { .. })
    ));
    assert!(matches!(
        find_node(&f.store, &snap.tree, ".git/HEAD").unwrap(),
        Some(Node::File { .. })
    ));
    assert_eq!(
        find_node(&f.store, &snap.tree, "entry").unwrap(),
        Some(Node::Link {
            name: "entry".into(),
            target: "src/main.rs".into()
        })
    );
    assert_eq!(
        find_node(&f.store, &snap.tree, "node_modules").unwrap(),
        None
    );
    assert_eq!(snap.files, 3);
}

#[test]
fn agent_actions_that_change_nothing_fold_into_the_last_snapshot() {
    let f = fixture();
    write(&f.root, "README.md", "hello\n");
    let project = f.store.project(&f.root).unwrap();
    let settings = Settings::default();

    let first = take(&f.store, &project, &settings, Kind::Agent, action("ls")).unwrap();
    let second = take(
        &f.store,
        &project,
        &settings,
        Kind::Agent,
        action("cat README.md"),
    )
    .unwrap();
    write(&f.root, "README.md", "changed\n");
    let third = take(
        &f.store,
        &project,
        &settings,
        Kind::Agent,
        action("rm -rf src"),
    )
    .unwrap();

    assert_eq!(second.id, first.id);
    let details: Vec<&str> = second
        .actions
        .iter()
        .map(|action| action.detail.as_str())
        .collect();
    assert_eq!(details, ["ls", "cat README.md"]);
    assert_eq!(third.id, first.id + 1);
    assert_eq!(project.ids().unwrap(), [1, 2]);
}

#[test]
fn rewrite_with_same_size_and_mtime_is_still_detected() {
    let f = fixture();
    write(&f.root, "notes.txt", "aaaa");
    let past = SystemTime::now() - Duration::from_secs(3600);
    set_mtime(&f.root.join("notes.txt"), past);
    let project = f.store.project(&f.root).unwrap();
    let settings = Settings {
        racy_window_ns: -60_000_000_000,
        ..Settings::default()
    };

    let before = take(&f.store, &project, &settings, Kind::Manual, None).unwrap();
    write(&f.root, "notes.txt", "bbbb");
    set_mtime(&f.root.join("notes.txt"), past);
    let after = take(&f.store, &project, &settings, Kind::Manual, None).unwrap();

    assert_ne!(before.tree, after.tree);
    let Some(Node::File { hash, .. }) = find_node(&f.store, &after.tree, "notes.txt").unwrap()
    else {
        panic!("notes.txt is missing from the snapshot");
    };
    assert_eq!(fs::read(f.store.object_path(&hash)).unwrap(), b"bbbb");
}

#[test]
fn files_over_the_limit_are_skipped_when_copy_on_write_is_off() {
    let f = fixture();
    write(&f.root, "small.txt", "tiny");
    write(&f.root, "big.bin", &"x".repeat(100));
    let project = f.store.project(&f.root).unwrap();
    let settings = Settings {
        reflink: false,
        max_file_bytes: 50,
        ..Settings::default()
    };

    let snap = take(&f.store, &project, &settings, Kind::Manual, None).unwrap();

    assert_eq!(find_node(&f.store, &snap.tree, "big.bin").unwrap(), None);
    assert!(matches!(
        find_node(&f.store, &snap.tree, "small.txt").unwrap(),
        Some(Node::File { .. })
    ));
    assert!(snap.skipped.iter().any(|note| note.starts_with("big.bin")));
}

#[test]
fn parallel_snapshots_get_distinct_ids() {
    let f = fixture();
    write(&f.root, "a.txt", "a");
    let project = f.store.project(&f.root).unwrap();
    let settings = Settings::default();

    let mut ids: Vec<u64> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    take(&f.store, &project, &settings, Kind::Manual, None)
                        .unwrap()
                        .id
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    ids.sort_unstable();

    assert_eq!(ids, (1..=8).collect::<Vec<u64>>());
}

#[test]
fn old_snapshots_are_pruned_past_the_keep_limit() {
    let f = fixture();
    let project = f.store.project(&f.root).unwrap();
    let settings = Settings {
        keep: 3,
        ..Settings::default()
    };

    for count in 0..5 {
        write(&f.root, "counter.txt", &count.to_string());
        take(&f.store, &project, &settings, Kind::Manual, None).unwrap();
    }

    assert_eq!(project.ids().unwrap(), [3, 4, 5]);
}

#[test]
fn find_root_needs_a_git_work_tree() {
    let temp = tempfile::tempdir().unwrap();
    let plain = temp.path().join("plain/deep");
    fs::create_dir_all(&plain).unwrap();
    let repo = temp.path().join("repo");
    write(&repo, ".git/HEAD", "ref: refs/heads/main\n");
    fs::create_dir_all(repo.join("src/nested")).unwrap();

    assert_eq!(find_root(&plain), None);
    assert_eq!(
        find_root(&repo.join("src/nested/not-created-yet.rs")),
        Some(repo.canonicalize().unwrap())
    );
}
