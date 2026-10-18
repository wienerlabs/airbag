use airbag::gc::{collect, collect_older_than};
use airbag::restore;
use airbag::snapshot::{Node, Settings, find_node, take};
use airbag::store::{Kind, Store};
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn blob(store: &Store, tree: &str, path: &str) -> String {
    match find_node(store, tree, path).unwrap() {
        Some(Node::File { hash, .. }) => hash,
        other => panic!("{path} is not a file in the snapshot: {other:?}"),
    }
}

#[test]
fn gc_drops_copies_only_pruned_snapshots_used_and_undo_still_works() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    write(&root, ".git/HEAD", "ref: refs/heads/main\n");
    write(&root, "notes.txt", "first version");
    let store = Store::at(temp.path().join("store")).unwrap();
    let project = store.project(&root).unwrap();
    let settings = Settings {
        keep: 1,
        ..Settings::default()
    };

    let old = take(&store, &project, &settings, Kind::Manual, None).unwrap();
    let old_blob = blob(&store, &old.tree, "notes.txt");
    write(&root, "notes.txt", "second version");
    let kept = take(&store, &project, &settings, Kind::Manual, None).unwrap();
    let kept_blob = blob(&store, &kept.tree, "notes.txt");

    let collected = collect_older_than(&store, -10).unwrap();

    assert_eq!(project.ids().unwrap(), [kept.id]);
    assert!(collected.removed >= 2, "{collected:?}");
    assert!(!store.object_path(&old_blob).exists());
    assert!(store.object_path(&kept_blob).exists());
    write(&root, "notes.txt", "broken by an agent");
    restore::restore(&store, &project, &settings, &kept, None).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("notes.txt")).unwrap(),
        "second version"
    );
}

#[test]
fn gc_keeps_fresh_copies_a_snapshot_in_progress_may_need() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::at(temp.path().join("store")).unwrap();
    let orphan = store.put_bytes(b"written a moment ago").unwrap();

    let collected = collect(&store).unwrap();

    assert_eq!(collected.removed, 0);
    assert!(store.object_path(&orphan).exists());
}
