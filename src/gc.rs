use crate::snapshot::{Node, load_tree};
use crate::store::Store;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::{SystemTime, UNIX_EPOCH};

pub const GRACE_SECONDS: i64 = 3600;

#[derive(Debug, Default)]
pub struct Collected {
    pub removed: u64,
    pub freed_bytes: u64,
    pub kept: u64,
}

pub fn collect(store: &Store) -> Result<Collected> {
    collect_older_than(store, GRACE_SECONDS)
}

pub fn collect_older_than(store: &Store, grace_seconds: i64) -> Result<Collected> {
    let mut live = HashSet::new();
    for project in store.projects()? {
        for snapshot in project.snapshots()? {
            mark(store, &snapshot.tree, &mut live).with_context(|| {
                format!(
                    "snapshot #{} of {} is damaged; nothing was deleted",
                    snapshot.id,
                    project.root.display()
                )
            })?;
        }
        live.extend(project.load_index().into_values().map(|cached| cached.hash));
    }
    let cutoff = now_seconds() - grace_seconds;
    let mut collected = Collected::default();
    for shard in fs::read_dir(store.home().join("objects"))? {
        let shard = shard?;
        let prefix = shard.file_name().to_string_lossy().into_owned();
        for object in fs::read_dir(shard.path())? {
            let object = object?;
            let hash = format!("{prefix}{}", object.file_name().to_string_lossy());
            let meta = object.metadata()?;
            if live.contains(&hash) || meta.ctime() >= cutoff {
                collected.kept += 1;
                continue;
            }
            fs::remove_file(object.path())?;
            collected.removed += 1;
            collected.freed_bytes += meta.len();
        }
    }
    for temp in fs::read_dir(store.home().join("tmp"))? {
        let temp = temp?;
        if temp.metadata()?.ctime() < cutoff {
            let _ = fs::remove_file(temp.path());
        }
    }
    Ok(collected)
}

fn mark(store: &Store, tree: &str, live: &mut HashSet<String>) -> Result<()> {
    if !live.insert(tree.to_string()) {
        return Ok(());
    }
    for node in load_tree(store, tree)? {
        match node {
            Node::Dir { hash, .. } => mark(store, &hash, live)?,
            Node::File { hash, .. } => {
                live.insert(hash);
            }
            Node::Link { .. } => {}
        }
    }
    Ok(())
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
