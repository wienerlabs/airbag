use crate::store::{
    Action, Cached, Index, Kind, Project, Snapshot, Stamp, Store, home_dir, mtime_ns, now_ms,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, Metadata};
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".turbo",
    ".gradle",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".DS_Store",
];
const MAX_ACTIONS_PER_SNAPSHOT: usize = 50;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Node {
    File {
        name: String,
        mode: u32,
        hash: String,
        size: u64,
        mtime_ns: i64,
    },
    Dir {
        name: String,
        mode: u32,
        hash: String,
    },
    Link {
        name: String,
        target: String,
    },
}

impl Node {
    pub fn name(&self) -> &str {
        match self {
            Node::File { name, .. } | Node::Dir { name, .. } | Node::Link { name, .. } => name,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub excludes: Vec<String>,
    pub max_file_bytes: u64,
    pub max_files: u64,
    pub reflink: bool,
    pub keep: usize,
    pub racy_window_ns: i64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            excludes: DEFAULT_EXCLUDES
                .iter()
                .map(|name| name.to_string())
                .collect(),
            max_file_bytes: 1 << 30,
            max_files: 200_000,
            reflink: true,
            keep: 500,
            racy_window_ns: 2_000_000_000,
        }
    }
}

impl Settings {
    pub fn from_env() -> Settings {
        let defaults = Settings::default();
        Settings {
            excludes: match std::env::var("AIRBAG_EXCLUDE") {
                Ok(list) => list
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(String::from)
                    .collect(),
                Err(_) => defaults.excludes,
            },
            max_file_bytes: env_number("AIRBAG_MAX_FILE_BYTES").unwrap_or(defaults.max_file_bytes),
            max_files: env_number("AIRBAG_MAX_FILES").unwrap_or(defaults.max_files),
            reflink: std::env::var_os("AIRBAG_NO_REFLINK").is_none(),
            keep: env_number("AIRBAG_KEEP").map_or(defaults.keep, |keep| keep as usize),
            racy_window_ns: defaults.racy_window_ns,
        }
    }
}

fn env_number(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

pub fn find_root(start: &Path) -> Option<PathBuf> {
    let start = start.ancestors().find_map(|dir| dir.canonicalize().ok())?;
    let root = start
        .ancestors()
        .find(|dir| dir.join(".git").exists())?
        .to_path_buf();
    let home = home_dir().ok().and_then(|home| home.canonicalize().ok());
    if root.parent().is_none() || home.as_deref() == Some(root.as_path()) {
        return None;
    }
    Some(root)
}

pub fn load_tree(store: &Store, hash: &str) -> Result<Vec<Node>> {
    let nodes: Vec<Node> = serde_json::from_slice(&store.read_object(hash)?)
        .with_context(|| format!("tree {hash} is unreadable"))?;
    if let Some(bad) = nodes
        .iter()
        .map(Node::name)
        .find(|name| !is_plain_name(name))
    {
        bail!("tree {hash} holds an unsafe entry name {bad:?}");
    }
    Ok(nodes)
}

pub fn find_node(store: &Store, tree: &str, path: &str) -> Result<Option<Node>> {
    let mut hash = tree.to_string();
    let mut parts = path.split('/').filter(|part| !part.is_empty()).peekable();
    while let Some(part) = parts.next() {
        let Some(node) = load_tree(store, &hash)?
            .into_iter()
            .find(|node| node.name() == part)
        else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return Ok(Some(node));
        }
        match node {
            Node::Dir { hash: child, .. } => hash = child,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

pub fn hash_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    hash_reader(file)
}

fn hash_reader(file: fs::File) -> Result<String> {
    Ok(blake3::Hasher::new()
        .update_reader(file)?
        .finalize()
        .to_hex()
        .to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Store,
    Scan,
}

pub struct Capture {
    pub tree: String,
    pub files: u64,
    pub bytes: u64,
    pub skipped: Vec<String>,
}

pub fn capture(
    store: &Store,
    project: &Project,
    settings: &Settings,
    mode: Mode,
) -> Result<Capture> {
    let mut walker = Walker {
        store,
        settings,
        mode,
        cached: project.load_index(),
        fresh: Index::new(),
        settled_before_ns: (now_ms() as i64) * 1_000_000 - settings.racy_window_ns,
        store_home: store.home().canonicalize().ok(),
        files: 0,
        bytes: 0,
        skipped: Vec::new(),
    };
    let tree = walker.dir(&project.root, "")?;
    if mode == Mode::Store {
        project.save_index(store, &walker.fresh)?;
    }
    Ok(Capture {
        tree,
        files: walker.files,
        bytes: walker.bytes,
        skipped: walker.skipped,
    })
}

pub fn take(
    store: &Store,
    project: &Project,
    settings: &Settings,
    kind: Kind,
    action: Option<Action>,
) -> Result<Snapshot> {
    let _lock = project.lock()?;
    let capture = capture(store, project, settings, Mode::Store)?;
    let ids = project.ids()?;
    if kind == Kind::Agent
        && let Some(&last_id) = ids.last()
    {
        let mut last = project.load(last_id)?;
        if last.kind != Kind::Undo && last.tree == capture.tree {
            if let Some(action) = action {
                last.actions.push(action);
                let overflow = last.actions.len().saturating_sub(MAX_ACTIONS_PER_SNAPSHOT);
                last.actions.drain(..overflow);
                project.save(store, &last)?;
            }
            return Ok(last);
        }
    }
    let snapshot = Snapshot {
        id: ids.last().map_or(1, |last| last + 1),
        kind,
        created_ms: now_ms(),
        tree: capture.tree,
        files: capture.files,
        bytes: capture.bytes,
        actions: action.into_iter().collect(),
        skipped: capture.skipped,
    };
    project.save(store, &snapshot)?;
    let surplus = (ids.len() + 1).saturating_sub(settings.keep.max(1));
    for &old in &ids[..surplus.min(ids.len())] {
        project.forget(old)?;
    }
    Ok(snapshot)
}

enum Preserved {
    Kept,
    TooBig,
    Vanished,
}

struct Walker<'a> {
    store: &'a Store,
    settings: &'a Settings,
    mode: Mode,
    cached: Index,
    fresh: Index,
    settled_before_ns: i64,
    store_home: Option<PathBuf>,
    files: u64,
    bytes: u64,
    skipped: Vec<String>,
}

impl Walker<'_> {
    fn dir(&mut self, dir: &Path, rel: &str) -> Result<String> {
        let mut items = match fs::read_dir(dir) {
            Ok(items) => items.collect::<std::io::Result<Vec<_>>>()?,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                self.skipped.push(format!("{rel}/ (permission denied)"));
                Vec::new()
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", dir.display()));
            }
        };
        items.sort_by_key(|item| item.file_name());
        let mut nodes = Vec::with_capacity(items.len());
        for item in items {
            let path = item.path();
            let Ok(name) = item.file_name().into_string() else {
                self.skipped
                    .push(format!("{} (name is not UTF-8)", path.display()));
                continue;
            };
            if self.settings.excludes.contains(&name) {
                continue;
            }
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let kind = meta.file_type();
            if kind.is_dir() {
                if self.store_home.as_deref() == Some(path.as_path()) {
                    continue;
                }
                let hash = self.dir(&path, &child)?;
                nodes.push(Node::Dir {
                    name,
                    mode: permission_bits(&meta),
                    hash,
                });
            } else if kind.is_symlink() {
                match fs::read_link(&path) {
                    Ok(target) => match target.into_os_string().into_string() {
                        Ok(target) => nodes.push(Node::Link { name, target }),
                        Err(_) => self
                            .skipped
                            .push(format!("{child} (link target is not UTF-8)")),
                    },
                    Err(error) if error.kind() == ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                }
            } else if kind.is_file() {
                self.files += 1;
                if self.files > self.settings.max_files {
                    bail!(
                        "this project has more than {} files; exclude big folders with AIRBAG_EXCLUDE or raise AIRBAG_MAX_FILES",
                        self.settings.max_files
                    );
                }
                if let Some(hash) = self.file(&path, &child, &meta)? {
                    self.bytes += meta.len();
                    nodes.push(Node::File {
                        name,
                        mode: permission_bits(&meta),
                        hash,
                        size: meta.len(),
                        mtime_ns: mtime_ns(&meta),
                    });
                }
            }
        }
        self.store.put_bytes(&serde_json::to_vec(&nodes)?)
    }

    fn file(&mut self, path: &Path, rel: &str, meta: &Metadata) -> Result<Option<String>> {
        let stamp = Stamp::of(meta);
        if let Some(cached) = self.cached.get(rel)
            && cached.stamp == stamp
        {
            let hash = cached.hash.clone();
            if self.mode == Mode::Store {
                self.fresh.insert(rel.to_string(), cached.clone());
            }
            return Ok(Some(hash));
        }
        if self.mode == Mode::Scan {
            return match fs::File::open(path) {
                Ok(file) => hash_reader(file).map(Some),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            };
        }
        let temp = self.store.temp_path();
        match self.preserve(path, rel, meta.len(), &temp)? {
            Preserved::Kept => {}
            Preserved::Vanished => return Ok(None),
            Preserved::TooBig => {
                self.skipped.push(format!(
                    "{rel} ({} bytes, over AIRBAG_MAX_FILE_BYTES on a disk without copy-on-write)",
                    meta.len()
                ));
                return Ok(None);
            }
        }
        let hash = hash_file(&temp)?;
        self.store.adopt(&temp, &hash)?;
        let settled = stamp.ctime_ns < self.settled_before_ns;
        let untouched = fs::symlink_metadata(path).is_ok_and(|after| Stamp::of(&after) == stamp);
        if settled && untouched {
            self.fresh.insert(
                rel.to_string(),
                Cached {
                    stamp,
                    hash: hash.clone(),
                },
            );
        }
        Ok(Some(hash))
    }

    fn preserve(&self, path: &Path, rel: &str, len: u64, temp: &Path) -> Result<Preserved> {
        if self.settings.reflink && reflink_copy::reflink(path, temp).is_ok() {
            return Ok(Preserved::Kept);
        }
        let _ = fs::remove_file(temp);
        if rel.starts_with(".git/objects/") && fs::hard_link(path, temp).is_ok() {
            return Ok(Preserved::Kept);
        }
        if len > self.settings.max_file_bytes {
            return Ok(Preserved::TooBig);
        }
        match fs::copy(path, temp) {
            Ok(_) => Ok(Preserved::Kept),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Preserved::Vanished),
            Err(error) => Err(error).with_context(|| format!("cannot copy {rel}")),
        }
    }
}

fn permission_bits(meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o7777
}
