use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct Store {
    home: PathBuf,
}

pub struct Project {
    pub root: PathBuf,
    dir: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Agent,
    Manual,
    Undo,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub agent: String,
    pub tool: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub at_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub id: u64,
    pub kind: Kind,
    pub created_ms: u64,
    pub tree: String,
    pub files: u64,
    pub bytes: u64,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "m")]
    pub mtime_ns: i64,
    #[serde(rename = "c")]
    pub ctime_ns: i64,
    #[serde(rename = "i")]
    pub ino: u64,
    #[serde(rename = "p")]
    pub mode: u32,
}

impl Stamp {
    pub fn of(meta: &Metadata) -> Stamp {
        Stamp {
            size: meta.size(),
            mtime_ns: mtime_ns(meta),
            ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
            ino: meta.ino(),
            mode: meta.mode(),
        }
    }
}

pub fn mtime_ns(meta: &Metadata) -> i64 {
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Cached {
    #[serde(rename = "t")]
    pub stamp: Stamp,
    #[serde(rename = "h")]
    pub hash: String,
}

pub type Index = HashMap<String, Cached>;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

pub fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let count = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{count}", std::process::id())
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

impl Store {
    pub fn open() -> Result<Store> {
        let home = match std::env::var_os("AIRBAG_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => home_dir()?.join(".airbag"),
        };
        Store::at(home)
    }

    pub fn at(home: PathBuf) -> Result<Store> {
        for sub in ["objects", "projects", "tmp"] {
            let dir = home.join(sub);
            fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Ok(Store { home })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn object_path(&self, hash: &str) -> PathBuf {
        self.home.join("objects").join(&hash[..2]).join(&hash[2..])
    }

    pub fn temp_path(&self) -> PathBuf {
        self.home.join("tmp").join(unique_suffix())
    }

    pub fn adopt(&self, temp: &Path, hash: &str) -> Result<()> {
        let dest = self.object_path(hash);
        if dest.exists() {
            fs::remove_file(temp)?;
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(temp, &dest)?;
        Ok(())
    }

    pub fn put_bytes(&self, bytes: &[u8]) -> Result<String> {
        let hash = blake3::hash(bytes).to_hex().to_string();
        if !self.object_path(&hash).exists() {
            let temp = self.temp_path();
            fs::write(&temp, bytes)?;
            self.adopt(&temp, &hash)?;
        }
        Ok(hash)
    }

    pub fn read_object(&self, hash: &str) -> Result<Vec<u8>> {
        fs::read(self.object_path(hash))
            .with_context(|| format!("object {hash} is missing from the store"))
    }

    pub fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let temp = self.temp_path();
        fs::write(&temp, bytes)?;
        fs::rename(&temp, path)?;
        Ok(())
    }

    pub fn project(&self, root: &Path) -> Result<Project> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", root.display()))?;
        let id = blake3::hash(root.as_os_str().as_bytes()).to_hex();
        let dir = self.home.join("projects").join(&id.as_str()[..16]);
        fs::create_dir_all(dir.join("snapshots"))?;
        let marker = dir.join("root");
        if !marker.exists() {
            fs::write(&marker, root.as_os_str().as_bytes())?;
        }
        Ok(Project { root, dir })
    }
}

impl Project {
    pub fn lock(&self) -> Result<File> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
        file.lock()?;
        Ok(file)
    }

    fn record_path(&self, id: u64) -> PathBuf {
        self.dir.join("snapshots").join(format!("{id:010}.json"))
    }

    pub fn ids(&self) -> Result<Vec<u64>> {
        let mut ids: Vec<u64> = fs::read_dir(self.dir.join("snapshots"))?
            .filter_map(|entry| {
                entry
                    .ok()?
                    .file_name()
                    .to_str()?
                    .strip_suffix(".json")?
                    .parse()
                    .ok()
            })
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    pub fn load(&self, id: u64) -> Result<Snapshot> {
        let bytes = fs::read(self.record_path(id))
            .with_context(|| format!("there is no snapshot #{id}"))?;
        serde_json::from_slice(&bytes).with_context(|| format!("snapshot #{id} is unreadable"))
    }

    pub fn snapshots(&self) -> Result<Vec<Snapshot>> {
        self.ids()?.into_iter().map(|id| self.load(id)).collect()
    }

    pub fn save(&self, store: &Store, snapshot: &Snapshot) -> Result<()> {
        store.write_atomic(
            &self.record_path(snapshot.id),
            &serde_json::to_vec_pretty(snapshot)?,
        )
    }

    pub fn forget(&self, id: u64) -> Result<()> {
        fs::remove_file(self.record_path(id))?;
        Ok(())
    }

    pub fn load_index(&self) -> Index {
        fs::read(self.dir.join("index.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save_index(&self, store: &Store, index: &Index) -> Result<()> {
        store.write_atomic(&self.dir.join("index.json"), &serde_json::to_vec(index)?)
    }
}
