use crate::snapshot::{
    self, Mode, Node, Settings, capture, find_node, is_plain_name, join_rel, load_tree,
};
use crate::store::{Action, Kind, Project, Snapshot, Store, now_ms, unique_suffix};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub status: Status,
    pub path: String,
}

#[derive(Default, Debug)]
pub struct Report {
    pub written: u64,
    pub removed: u64,
    pub kept: Vec<String>,
}

pub struct Restored {
    pub safety: Snapshot,
    pub report: Report,
}

pub fn current_tree(store: &Store, project: &Project, settings: &Settings) -> Result<String> {
    Ok(capture(store, project, settings, Mode::Scan)?.tree)
}

pub fn diff(store: &Store, from: &str, to: &str, scope: Option<&str>) -> Result<Vec<Change>> {
    let (parent, before) = entries_at(store, from, scope)?;
    let (_, after) = entries_at(store, to, scope)?;
    let mut changes = Vec::new();
    diff_lists(store, &parent, &before, &after, &mut changes)?;
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

pub fn undo_target(
    store: &Store,
    project: &Project,
    current: &str,
    scope: Option<&str>,
) -> Result<Option<Snapshot>> {
    for snapshot in project.snapshots()?.into_iter().rev() {
        if snapshot.kind != Kind::Undo && !diff(store, &snapshot.tree, current, scope)?.is_empty() {
            return Ok(Some(snapshot));
        }
    }
    Ok(None)
}

pub fn redo_target(project: &Project) -> Result<Option<Snapshot>> {
    Ok(project
        .snapshots()?
        .into_iter()
        .rev()
        .find(|snapshot| snapshot.kind == Kind::Undo))
}

pub fn restore(
    store: &Store,
    project: &Project,
    settings: &Settings,
    target: &Snapshot,
    scope: Option<&str>,
) -> Result<Restored> {
    let _lock = project.lock()?;
    let (parent, wanted) = entries_at(store, &target.tree, scope)?;
    ensure_objects(store, &wanted)?;
    enter_folders(&project.root, &parent, false)?;
    let detail = match scope {
        Some(path) => format!("state before restoring {path} from #{}", target.id),
        None => format!("state before restoring #{}", target.id),
    };
    let action = Action {
        agent: "airbag".into(),
        tool: "undo".into(),
        detail,
        session: None,
        at_ms: now_ms(),
    };
    let safety = snapshot::take_locked(store, project, settings, Kind::Undo, Some(action))?;
    let (_, current) = entries_at(store, &safety.tree, scope)?;
    let base = enter_folders(&project.root, &parent, true)?;
    let base_mode = fs::symlink_metadata(&base)?.permissions().mode() & 0o7777;
    make_writable(&base)?;
    let mut restorer = Restorer {
        store,
        report: Report::default(),
    };
    let synced = restorer.sync(&base, &parent, &current, &wanted);
    set_mode(&base, base_mode)?;
    synced?;
    Ok(Restored {
        safety,
        report: restorer.report,
    })
}

fn enter_folders(root: &Path, parent: &str, create: bool) -> Result<PathBuf> {
    let mut base = root.to_path_buf();
    for part in parent.split('/').filter(|part| !part.is_empty()) {
        base.push(part);
        match fs::symlink_metadata(&base) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "{} is no longer a folder (a link or a file took its place); run `airbag undo` without --path to restore it",
                base.strip_prefix(root).unwrap_or(&base).display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound && !create => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&base)
                    .with_context(|| format!("cannot create {}", base.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", base.display()));
            }
        }
    }
    Ok(base)
}

fn entries_at(store: &Store, tree: &str, scope: Option<&str>) -> Result<(String, Vec<Node>)> {
    let Some(scope) = scope else {
        return Ok((String::new(), load_tree(store, tree)?));
    };
    if !scope.split('/').all(is_plain_name) {
        bail!("{scope:?} is not a path inside the project");
    }
    let (parent, leaf) = scope.rsplit_once('/').unwrap_or(("", scope));
    let siblings = if parent.is_empty() {
        load_tree(store, tree)?
    } else {
        match find_node(store, tree, parent)? {
            Some(Node::Dir { hash, .. }) => load_tree(store, &hash)?,
            _ => Vec::new(),
        }
    };
    let entries = siblings
        .into_iter()
        .filter(|node| node.name() == leaf)
        .collect();
    Ok((parent.to_string(), entries))
}

fn ensure_objects(store: &Store, nodes: &[Node]) -> Result<()> {
    for node in nodes {
        match node {
            Node::Dir { hash, .. } => ensure_objects(store, &load_tree(store, hash)?)?,
            Node::File { hash, name, .. } if !store.object_path(hash).exists() => {
                bail!("the stored copy of {name} is missing; nothing was changed");
            }
            _ => {}
        }
    }
    Ok(())
}

fn same(a: &Node, b: &Node) -> bool {
    match (a, b) {
        (
            Node::File {
                hash: x, mode: m, ..
            },
            Node::File {
                hash: y, mode: n, ..
            },
        )
        | (
            Node::Dir {
                hash: x, mode: m, ..
            },
            Node::Dir {
                hash: y, mode: n, ..
            },
        ) => x == y && m == n,
        (Node::Link { target: x, .. }, Node::Link { target: y, .. }) => x == y,
        _ => false,
    }
}

fn diff_lists(
    store: &Store,
    rel: &str,
    before: &[Node],
    after: &[Node],
    out: &mut Vec<Change>,
) -> Result<()> {
    let old: HashMap<&str, &Node> = before.iter().map(|node| (node.name(), node)).collect();
    let new: HashSet<&str> = after.iter().map(Node::name).collect();
    for node in before.iter().filter(|node| !new.contains(node.name())) {
        leaves(store, rel, node, Status::Deleted, out)?;
    }
    for node in after {
        let path = join_rel(rel, node.name());
        match (old.get(node.name()).copied(), node) {
            (None, added) => leaves(store, rel, added, Status::Added, out)?,
            (Some(was), now) if same(was, now) => {}
            (
                Some(Node::Dir {
                    hash: was,
                    mode: old_mode,
                    ..
                }),
                Node::Dir {
                    hash: now,
                    mode: new_mode,
                    ..
                },
            ) => {
                if old_mode != new_mode {
                    out.push(Change {
                        status: Status::Modified,
                        path: format!("{path}/"),
                    });
                }
                if was != now {
                    let (was, now) = (load_tree(store, was)?, load_tree(store, now)?);
                    diff_lists(store, &path, &was, &now, out)?;
                }
            }
            (Some(_), _) => out.push(Change {
                status: Status::Modified,
                path,
            }),
        }
    }
    Ok(())
}

fn leaves(
    store: &Store,
    rel: &str,
    node: &Node,
    status: Status,
    out: &mut Vec<Change>,
) -> Result<()> {
    let path = join_rel(rel, node.name());
    match node {
        Node::Dir { hash, .. } => {
            let children = load_tree(store, hash)?;
            if children.is_empty() {
                out.push(Change {
                    status,
                    path: format!("{path}/"),
                });
            }
            for child in &children {
                leaves(store, &path, child, status, out)?;
            }
        }
        _ => out.push(Change { status, path }),
    }
    Ok(())
}

struct Restorer<'a> {
    store: &'a Store,
    report: Report,
}

impl Restorer<'_> {
    fn sync(&mut self, dir: &Path, rel: &str, current: &[Node], wanted: &[Node]) -> Result<()> {
        let have: HashMap<&str, &Node> = current.iter().map(|node| (node.name(), node)).collect();
        let keep: HashSet<&str> = wanted.iter().map(Node::name).collect();
        for node in current.iter().filter(|node| !keep.contains(node.name())) {
            self.remove(dir, rel, node)?;
        }
        for node in wanted {
            let path = dir.join(node.name());
            let child = join_rel(rel, node.name());
            match (have.get(node.name()).copied(), node) {
                (Some(old), new) if same(old, new) => {}
                (
                    Some(Node::Dir { hash: old, .. }),
                    Node::Dir {
                        hash: new, mode, ..
                    },
                ) => {
                    make_writable(&path)?;
                    if old != new {
                        let (old, new) = (load_tree(self.store, old)?, load_tree(self.store, new)?);
                        self.sync(&path, &child, &old, &new)?;
                    }
                    set_mode(&path, *mode)?;
                }
                (Some(old @ Node::Dir { .. }), new) | (Some(old), new @ Node::Dir { .. }) => {
                    self.remove(dir, rel, old)?;
                    self.create(&path, &child, new, false)?;
                }
                (Some(_), new) => self.create(&path, &child, new, true)?,
                (None, new) => self.create(&path, &child, new, false)?,
            }
        }
        Ok(())
    }

    fn remove(&mut self, dir: &Path, rel: &str, node: &Node) -> Result<()> {
        let path = dir.join(node.name());
        let child = join_rel(rel, node.name());
        if let Node::Dir { hash, .. } = node {
            if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                return Ok(());
            }
            make_writable(&path)?;
            for entry in load_tree(self.store, hash)? {
                self.remove(&path, &child, &entry)?;
            }
            return match fs::remove_dir(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => {
                    self.report
                        .kept
                        .push(format!("{child}/ still holds files airbag does not track"));
                    Ok(())
                }
                Err(error) => Err(error).with_context(|| format!("cannot remove {child}/")),
            };
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                self.report.removed += 1;
                Ok(())
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("cannot remove {child}")),
        }
    }

    fn create(&mut self, path: &Path, rel: &str, node: &Node, replace: bool) -> Result<()> {
        let occupied = fs::symlink_metadata(path).ok();
        match node {
            Node::Dir { hash, mode, .. } => {
                match occupied {
                    Some(meta) if meta.is_dir() => make_writable(path)?,
                    Some(_) => {
                        self.move_aside(path, rel)?;
                        fs::create_dir(path).with_context(|| format!("cannot create {rel}/"))?;
                    }
                    None => {
                        fs::create_dir(path).with_context(|| format!("cannot create {rel}/"))?
                    }
                }
                self.sync(path, rel, &[], &load_tree(self.store, hash)?)?;
                set_mode(path, *mode)?;
            }
            Node::File { hash, mode, .. } => {
                if occupied.is_some() && !replace {
                    self.move_aside(path, rel)?;
                }
                let temp = sibling_temp(path);
                reflink_copy::reflink_or_copy(self.store.object_path(hash), &temp)
                    .with_context(|| format!("cannot restore {rel}"))?;
                fs::set_permissions(&temp, fs::Permissions::from_mode(*mode))?;
                fs::rename(&temp, path).with_context(|| format!("cannot restore {rel}"))?;
                self.report.written += 1;
            }
            Node::Link { target, .. } => {
                if occupied.is_some() && !replace {
                    self.move_aside(path, rel)?;
                }
                let temp = sibling_temp(path);
                symlink(target, &temp)?;
                fs::rename(&temp, path).with_context(|| format!("cannot restore {rel}"))?;
                self.report.written += 1;
            }
        }
        Ok(())
    }

    fn move_aside(&mut self, path: &Path, rel: &str) -> Result<()> {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let aside = path.with_file_name(format!("{name}.airbag-kept-{}", unique_suffix()));
        fs::rename(path, &aside).with_context(|| format!("cannot move {rel} out of the way"))?;
        self.report.kept.push(format!(
            "{rel} was not in any snapshot, moved to {}",
            aside.file_name().unwrap_or_default().to_string_lossy()
        ));
        Ok(())
    }
}

fn sibling_temp(path: &Path) -> PathBuf {
    path.with_file_name(format!(".airbag-{}", unique_suffix()))
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("cannot set permissions on {}", path.display()))
}

fn make_writable(path: &Path) -> Result<()> {
    let mode = fs::symlink_metadata(path)?.permissions().mode();
    if mode & 0o200 == 0 {
        set_mode(path, mode | 0o700)?;
    }
    Ok(())
}
