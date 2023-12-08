use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use futures::StreamExt;
use iroh::client;
use iroh::{
    client::{Doc, Iroh},
    rpc_protocol::ProviderService,
    sync_engine::LiveEvent,
};
use iroh_sync::{store::Query, AuthorId, NamespaceId};
use nfsserve::nfs::mode3;
use nfsserve::{
    nfs::{
        self, fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
    },
    vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities},
};
use quic_rpc::ServiceConnection;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, TimestampSecondsWithFrac};
use tokio::sync::RwLock;
use tokio_util::task::LocalPoolHandle;
use tracing::{debug, error, info, warn};

use crate::commands::mount_runner::perform_mount_and_wait_for_ctrlc;

use super::runtime::IrohWrapper;

const HOSTPORT: u32 = 11111;

// Directory
// - .dir.iroh
//   - fattr3
//
// File
// - file
// - .file.iroh
//   - fattr3

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attrs {
    /// The id of the file
    fileid: fileid3,
    /// Last access time
    #[serde_as(as = "TimestampSecondsWithFrac<String>")]
    atime: DateTime<Utc>,
    /// Last modification time
    #[serde_as(as = "TimestampSecondsWithFrac<String>")]
    mtime: DateTime<Utc>,
    /// Creation time
    #[serde_as(as = "TimestampSecondsWithFrac<String>")]
    ctime: DateTime<Utc>,
    /// size
    size: u64,
    /// Mode
    mode: mode3,
    ftype: FileType,
    /// The name
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum FileType {
    File,
    Directory,
}

impl From<FileType> for ftype3 {
    fn from(value: FileType) -> Self {
        match value {
            FileType::File => ftype3::NF3REG,
            FileType::Directory => ftype3::NF3DIR,
        }
    }
}

impl TryFrom<ftype3> for FileType {
    type Error = anyhow::Error;

    fn try_from(value: ftype3) -> std::result::Result<Self, Self::Error> {
        match value {
            ftype3::NF3REG => Ok(Self::File),
            ftype3::NF3DIR => Ok(Self::Directory),
            _ => Err(anyhow!("invalid ftype3: {:?}", value)),
        }
    }
}

impl From<Attrs> for fattr3 {
    fn from(value: Attrs) -> Self {
        fattr3 {
            ftype: value.ftype.into(),
            mode: value.mode,
            nlink: 1,
            uid: 507,
            gid: 507,
            size: value.size,
            used: value.size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: value.fileid,
            atime: to_nfstime(&value.atime),
            mtime: to_nfstime(&value.mtime),
            ctime: to_nfstime(&value.ctime),
        }
    }
}

impl Attrs {
    fn new_file(name: String, fileid: fileid3) -> Self {
        Attrs {
            fileid,
            atime: now(),
            mtime: now(),
            ctime: now(),
            size: 0,
            mode: 0o755,
            ftype: FileType::File,
            name,
        }
    }

    fn new_dir(name: String, fileid: fileid3) -> Self {
        Attrs {
            fileid,
            atime: now(),
            mtime: now(),
            ctime: now(),
            size: 0,
            mode: 0o777,
            ftype: FileType::Directory,
            name,
        }
    }
}

pub async fn exec<C>(
    iroh: &Iroh<C>,
    doc: NamespaceId,
    path: PathBuf,
    rt: LocalPoolHandle,
) -> Result<()>
where
    C: ServiceConnection<ProviderService>,
{
    let path = path.canonicalize()?;
    println!("mounting {} at {}", doc, path.display());
    let fs = IrohFs::new(iroh.clone(), doc, path.clone(), rt).await?;

    println!("fs prepared");
    perform_mount_and_wait_for_ctrlc(
        &path,
        fs,
        true,
        true,
        format!("127.0.0.1:{HOSTPORT}"),
        || {},
    )
    .await?;

    Ok(())
}

#[derive(Debug, Clone)]
enum FsContents {
    File {
        author: AuthorId,
    },
    Directory {
        author: AuthorId,
        content: Vec<fileid3>,
    },
}

impl FsContents {
    fn author(&self) -> &AuthorId {
        match self {
            FsContents::File { ref author } => author,
            FsContents::Directory { ref author, .. } => author,
        }
    }

    fn set_author(&mut self, new_author: AuthorId) {
        match self {
            FsContents::File { ref mut author } => {
                *author = new_author;
            }
            FsContents::Directory { ref mut author, .. } => {
                *author = new_author;
            }
        }
    }

    fn children(&self) -> &[fileid3] {
        match self {
            FsContents::File { .. } => {
                panic!("not a directory");
            }
            FsContents::Directory { ref content, .. } => content,
        }
    }

    fn children_mut(&mut self) -> &mut Vec<fileid3> {
        match self {
            FsContents::File { .. } => {
                panic!("not a directory");
            }
            FsContents::Directory {
                ref mut content, ..
            } => content,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct FsEntry {
    attr: Attrs,
    parent: fileid3,
    contents: FsContents,
}

impl FsEntry {
    fn new_file(
        name: &str,
        id: fileid3,
        parent: fileid3,
        content_len: u64,
        author: AuthorId,
        ts: Option<DateTime<Utc>>,
    ) -> Self {
        let mut attr = Attrs::new_file(name.into(), id);
        if let Some(ts) = ts {
            attr.ctime = ts;
            attr.mtime = ts;
        }
        attr.size = content_len;
        Self {
            attr,
            parent,
            contents: FsContents::File { author },
        }
    }

    fn new_dir(
        name: &str,
        id: fileid3,
        parent: fileid3,
        author: AuthorId,
        content: Vec<fileid3>,
    ) -> Self {
        Self {
            attr: Attrs::new_dir(name.into(), id),
            parent,
            contents: FsContents::Directory { author, content },
        }
    }

    fn is_file(&self) -> bool {
        matches!(self.contents, FsContents::File { .. })
    }
    fn is_dir(&self) -> bool {
        matches!(self.contents, FsContents::Directory { .. })
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn to_nfstime(ts: &DateTime<Utc>) -> nfstime3 {
    nfstime3 {
        seconds: ts.timestamp() as _,
        nseconds: ts.timestamp_nanos_opt().unwrap() as _,
    }
}

fn ts_to_chrono(micros: u64) -> DateTime<Utc> {
    let n = NaiveDateTime::from_timestamp_micros(micros as i64).unwrap();
    NaiveDateTime::and_utc(&n)
}

fn nfstime_to_chrono(ts: nfstime3) -> DateTime<Utc> {
    Utc.timestamp_opt(ts.seconds as i64, ts.nseconds).unwrap()
}

#[derive(Debug, Clone, Default)]
struct Fs(Arc<RwLock<InnerFs>>);

#[derive(Debug, Default)]
struct InnerFs {
    by_path: BTreeMap<PathBuf, fileid3>,
    by_id: BTreeMap<fileid3, FsEntry>,
}

impl InnerFs {
    fn push(&mut self, path: PathBuf, entry: FsEntry, parent_id: fileid3) -> Result<()> {
        let id = entry.attr.fileid;
        self.by_path.insert(path, id);
        self.by_id.insert(id, entry);

        let parent = self
            .by_id
            .get_mut(&parent_id)
            .ok_or_else(|| anyhow!("unknown parent {}", parent_id))?;
        debug!("updating parent {}", parent_id);
        parent.contents.children_mut().push(id);
        parent.attr.mtime = now();
        Ok(())
    }

    fn get_by_path(&self, path: impl AsRef<Path>) -> Option<&FsEntry> {
        self.by_path
            .get(path.as_ref())
            .and_then(|id| self.by_id.get(id))
    }

    fn get_by_path_mut(&mut self, path: impl AsRef<Path>) -> Option<&mut FsEntry> {
        self.by_path
            .get(path.as_ref())
            .and_then(|id| self.by_id.get_mut(id))
    }

    fn get_by_id(&self, id: fileid3) -> Option<&FsEntry> {
        self.by_id.get(&id)
    }

    fn get_by_id_mut(&mut self, id: fileid3) -> Option<&mut FsEntry> {
        self.by_id.get_mut(&id)
    }

    fn contains_by_id(&self, id: fileid3) -> bool {
        self.by_id.contains_key(&id)
    }

    fn contains_by_path(&self, path: impl AsRef<Path>) -> bool {
        self.by_path.contains_key(path.as_ref())
    }

    fn get_id_for_path(&self, path: impl AsRef<Path>) -> Option<fileid3> {
        self.by_path.get(path.as_ref()).map(|v| *v)
    }

    fn remove_by_path(&mut self, path: impl AsRef<Path>) {
        // remove from by_id
        if let Some(id) = self.by_path.remove(path.as_ref()) {
            // remove from by_path
            if let Some(entry) = self.by_id.remove(&id) {
                // update parent
                if let Some(parent_entry) = self.by_id.get_mut(&entry.parent) {
                    parent_entry.contents.children_mut().retain(|i| i != &id);
                    parent_entry.attr.mtime = now();
                }
            }
        }
    }

    fn update_by_path(&mut self, path: impl AsRef<Path>, author: AuthorId, content_len: u64) {
        if let Some(id) = self.by_path.get(path.as_ref()) {
            self.update_by_id(*id, author, content_len);
        }
    }

    fn update_by_id(&mut self, id: fileid3, author: AuthorId, content_len: u64) {
        if let Some(entry) = self.by_id.get_mut(&id) {
            let ts = now();
            entry.contents.set_author(author);
            entry.attr.mtime = ts;
            entry.attr.atime = ts;
            entry.attr.size = content_len;

            // update times for parents
            let parent = entry.parent;

            self.for_each_parent_mut(parent, |p| {
                p.attr.mtime = ts;
                p.attr.atime = ts;
            });
        }
    }

    fn for_each_parent_mut<F>(&mut self, id: fileid3, mut cb: F)
    where
        F: FnMut(&mut FsEntry),
    {
        let mut current_parent = id;
        loop {
            if let Some(p) = self.by_id.get_mut(&current_parent) {
                cb(p);
                current_parent = p.parent;
            } else {
                break;
            }
        }
    }

    fn for_each_parent<F>(&self, id: fileid3, mut cb: F)
    where
        F: FnMut(&FsEntry),
    {
        let mut current_parent = self.by_id.get(&id);
        while let Some(p) = current_parent {
            cb(p);
            if p.parent == 0 {
                break;
            }
            current_parent = self.by_id.get(&p.parent);
        }
    }

    fn parent_id_for_path(&self, path: impl AsRef<Path>) -> Option<fileid3> {
        let parent_path = path.as_ref().parent()?;
        let parent_id = self.by_path.get(parent_path)?;
        Some(*parent_id)
    }

    fn get_path_for_file_in_dir(
        &self,
        dirid: fileid3,
        filename: impl AsRef<[u8]>,
    ) -> Option<PathBuf> {
        let parent_path = self.get_path_for_id(dirid)?;
        let name = std::str::from_utf8(filename.as_ref()).ok()?;
        Some(parent_path.join(name))
    }

    fn get_path_for_id(&self, id: fileid3) -> Option<PathBuf> {
        let entry = self.by_id.get(&id)?;
        let name = entry.attr.name.clone();
        let mut parts = vec![name, "/".into()];
        self.for_each_parent(entry.parent, |p| {
            let name = p.attr.name.clone();
            if name != "/" {
                parts.push(name);
            }
        });

        let path = PathBuf::from_iter(parts.into_iter().rev());
        Some(path)
    }
}

fn safe_name(name: impl AsRef<[u8]>) -> String {
    std::string::String::from_utf8_lossy(name.as_ref()).into_owned()
}

#[derive(Debug, Clone)]
pub struct IrohFs<C>
where
    C: ServiceConnection<ProviderService>,
{
    iroh: Iroh<C>,
    doc: Doc<C>,
    fs: Fs,
    next_id: Arc<AtomicU64>,
    rootdir: fileid3,
    author: AuthorId,
    mount_path: PathBuf,
}

const IROH_DIR: &str = ".dir.iroh";
const IROH_FILE: &str = ".fil.iroh";
const HIDDEN_PREFIX: &str = ".hidden";

fn fs_entry_name_from_path(path: impl AsRef<Path>) -> Result<String> {
    let res = path
        .as_ref()
        .file_name()
        .ok_or_else(|| anyhow!("invalid filename"))?
        .to_str()
        .ok_or_else(|| anyhow!("invalid filename"))?
        .to_string();
    Ok(res)
}

fn key_to_path(key: impl AsRef<[u8]>) -> Result<PathBuf> {
    iroh::util::fs::key_to_path(key, None, None)
}

fn path_to_key(path: impl AsRef<Path>) -> std::result::Result<Bytes, nfsstat3> {
    iroh::util::fs::path_to_key(path, None, None).map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)
}

fn iroh_path_to_path(p: impl AsRef<Path>) -> Result<PathBuf> {
    let path = p.as_ref();
    let full_name = fs_entry_name_from_path(&path)?;
    let actual_name = if full_name.starts_with(IROH_FILE) {
        full_name.replace(&format!("{IROH_FILE}."), "")
    } else {
        full_name.replace(&format!("{IROH_DIR}."), "")
    };

    Ok(path.with_file_name(actual_name))
}

impl<C> IrohFs<C>
where
    C: ServiceConnection<ProviderService>,
{
    async fn new(
        iroh: Iroh<C>,
        doc_id: NamespaceId,
        mount_path: PathBuf,
        _rt: LocalPoolHandle,
    ) -> Result<Self> {
        let doc = iroh
            .docs
            .open(doc_id)
            .await?
            .ok_or_else(|| anyhow!("unknown document"))?;

        // TODO: better
        let author = iroh.authors.create().await?;

        let mut current_id = 0;
        let mut get_next_id = || {
            let id = current_id;
            current_id += 1;
            id
        };

        let mut fs = InnerFs::default();
        {
            let id = get_next_id();
            let base_entry = FsEntry::new_file("", id, 0, 0, author, None);
            fs.by_id.insert(id, base_entry);
            fs.by_path.insert(PathBuf::new(), id);
        }
        {
            let id = get_next_id();
            let path = PathBuf::from("/");
            let root_dir = FsEntry::new_dir("/", id, 0, author, Vec::new());
            fs.by_id.insert(id, root_dir);
            fs.by_path.insert(path, id);
        }

        let mut keys = doc
            .get_many(
                Query::single_latest_per_key().sort_direction(iroh_sync::store::SortDirection::Asc),
            )
            .await?;

        while let Some(entry) = keys.next().await {
            let entry = entry?;
            debug!("processing {:?}", std::str::from_utf8(entry.key()));
            if entry.key().starts_with(HIDDEN_PREFIX.as_bytes()) {
                info!("ignoring hidden: {:?}", std::str::from_utf8(entry.key()));
                continue;
            }

            let Ok(path) = key_to_path(entry.key()) else {
                warn!("ignoring invalid path: {:?}", entry.key());
                continue;
            };

            let name = fs_entry_name_from_path(&path)?;
            let is_iroh_file = name.starts_with(IROH_FILE);
            let is_iroh_dir = name.starts_with(IROH_DIR);

            debug!("{}: dir?{} - file?{}", name, is_iroh_dir, is_iroh_file);
            if !is_iroh_dir && !is_iroh_file {
                continue;
            }
            let Ok(attr_bytes) = iroh.blobs.read_to_bytes(entry.content_hash()).await else {
                warn!("skipping {}: content not available", path.display());
                continue;
            };
            let mut attr: Attrs =
                serde_json::from_slice(&attr_bytes).context("invalid attrs stored")?;

            let Some(parent_path) = path.parent() else {
                bail!("invalid root entry: {}", path.display());
            };

            // handle root
            let parent_id = fs
                .get_id_for_path(parent_path)
                .ok_or_else(|| anyhow!("missing parent for: {}", path.display()))?;

            debug!("parent {}:{}", parent_id, parent_path.display());
            if parent_id == 0 {
                // skip invalid entries
                continue;
            }

            let Ok(fs_path) = iroh_path_to_path(&path) else {
                error!("invalid path: {}", path.display());
                continue;
            };
            if fs.contains_by_path(&fs_path) {
                bail!("duplicate entry: {}", fs_path.display());
            }

            info!(
                "inserting {}: {} (is_dir: {})",
                name,
                fs_path.display(),
                is_iroh_dir
            );

            let id = get_next_id();
            attr.fileid = id;
            let contents = if is_iroh_dir {
                FsContents::Directory {
                    author: entry.author(),
                    content: Vec::new(),
                }
            } else {
                FsContents::File {
                    author: entry.author(),
                }
            };
            let entry = FsEntry {
                attr,
                parent: parent_id,
                contents,
            };
            fs.push(fs_path, entry, parent_id)?;
        }

        let fs = Fs(Arc::new(RwLock::new(fs)));

        let next_id = Arc::new(AtomicU64::new(current_id));
        let res = Self {
            fs,
            doc,
            rootdir: 1,
            next_id,
            iroh,
            author,
            mount_path,
        };
        let this = res.clone();
        tokio::task::spawn(async move { this.handle_changes().await });

        Ok(res)
    }
}

impl<C> IrohFs<C>
where
    C: ServiceConnection<ProviderService>,
{
    async fn handle_changes(&self) {
        let mut sub = match self.doc.subscribe().await {
            Ok(sub) => sub,
            Err(err) => {
                error!("failed to setup subscription: {:?}", err);
                return;
            }
        };

        while let Some(item) = sub.next().await {
            match item {
                Ok(event) => {
                    match event {
                        LiveEvent::InsertLocal { entry }
                        | LiveEvent::InsertRemote { entry, .. } => {
                            if entry.key().starts_with(HIDDEN_PREFIX.as_bytes()) {
                                info!("ignoring hidden: {:?}", std::str::from_utf8(entry.key()));
                                continue;
                            }

                            // insert into fs
                            let path = match key_to_path(entry.key()) {
                                Err(err) => {
                                    warn!("ignoring invalid key: {:?}: {:?}", entry.key(), err);
                                    continue;
                                }
                                Ok(path) => path,
                            };

                            let Ok(name) = fs_entry_name_from_path(&path) else {
                                continue;
                            };
                            debug!("change detected: {}", name);

                            let is_iroh_file = name.starts_with(IROH_FILE);
                            let is_iroh_dir = name.starts_with(IROH_DIR);

                            if is_iroh_dir || is_iroh_file {
                                // metadata update

                                let mut fs = self.fs.0.write().await;
                                let is_deletion = entry.content_len() == 0;
                                let Ok(fs_path) = iroh_path_to_path(&path) else {
                                    error!("invalid path: {}", path.display());
                                    continue;
                                };

                                debug!("processing {} (is_del?{})", fs_path.display(), is_deletion);

                                if is_deletion {
                                    info!("deleted: {}", path.display());
                                    fs.remove_by_path(&fs_path);
                                } else {
                                    let Ok(attr_bytes) =
                                        self.iroh.blobs.read_to_bytes(entry.content_hash()).await
                                    else {
                                        warn!("skipping {}: content not available", path.display());
                                        continue;
                                    };
                                    let Ok(mut attr) = serde_json::from_slice::<Attrs>(&attr_bytes)
                                    else {
                                        error!("found invalid attrs {}", path.display());
                                        continue;
                                    };

                                    // update attrs for size
                                    if !fs.contains_by_path(&fs_path) {
                                        // creation
                                        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                                        attr.fileid = id;
                                        let Some(parent_id) = fs.parent_id_for_path(&fs_path)
                                        else {
                                            error!(
                                                "no parent directory found for {}",
                                                fs_path.display()
                                            );
                                            continue;
                                        };
                                        let contents = if is_iroh_dir {
                                            FsContents::Directory {
                                                author: entry.author(),
                                                content: Vec::new(),
                                            }
                                        } else {
                                            FsContents::File {
                                                author: entry.author(),
                                            }
                                        };
                                        let entry = FsEntry {
                                            attr,
                                            parent: parent_id,
                                            contents,
                                        };
                                        if let Err(err) = fs.push(fs_path.clone(), entry, parent_id)
                                        {
                                            error!(
                                                "failed to insert: {}: {:?}",
                                                fs_path.display(),
                                                err
                                            );
                                        }

                                        info!("inserted {}: {}", name, fs_path.display());
                                    } else {
                                        // update
                                        info!("update: {}", fs_path.display());

                                        let Some(entry) = fs.get_by_path_mut(&fs_path) else {
                                            warn!("update for unknown path: {}", fs_path.display());
                                            continue;
                                        };
                                        entry.attr.atime = attr.atime;
                                        entry.attr.mtime = attr.mtime;
                                        entry.attr.ctime = attr.ctime;
                                        entry.attr.size = attr.size;
                                        entry.attr.mode = attr.mode;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(err) => {
                    warn!("event failure: {:?}", err);
                }
            }
        }
    }

    fn iroh_wrapper(&self) -> IrohWrapper {
        let any_iroh = (&self.iroh) as &dyn std::any::Any;
        if let Some(iroh) = any_iroh.downcast_ref::<client::mem::Iroh>() {
            return IrohWrapper::Mem(iroh.clone());
        }
        if let Some(iroh) = any_iroh.downcast_ref::<client::quic::Iroh>() {
            return IrohWrapper::Quic(iroh.clone());
        }
        panic!("unsupported iroh client");
    }

    async fn create_iroh_file(&self, path: impl AsRef<Path>, attrs: &Attrs) -> Result<()> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("not a valid file"))?
            .to_str()
            .ok_or_else(|| anyhow!("invalid filename"))?;
        let name = format!("{IROH_FILE}.{name}");
        let path = path.with_file_name(name);
        info!("creating iroh file {}", path.display());
        let key = path_to_key(path).map_err(|_| anyhow!("invalid path"))?;
        let attrs_bytes = serde_json::ser::to_vec_pretty(attrs)?;
        self.doc.set_bytes(self.author, key, attrs_bytes).await?;

        Ok(())
    }

    async fn create_iroh_dir(&self, path: impl AsRef<Path>, attrs: &Attrs) -> Result<()> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("not a valid file"))?
            .to_str()
            .ok_or_else(|| anyhow!("invalid filename"))?;
        let name = format!("{IROH_DIR}.{name}");
        let path = path.with_file_name(name);
        info!("creating iroh dir {}", path.display());
        let key = path_to_key(path).map_err(|_| anyhow!("invalid path"))?;
        let attrs_bytes = serde_json::ser::to_vec_pretty(attrs)?;
        self.doc.set_bytes(self.author, key, attrs_bytes).await?;

        Ok(())
    }

    async fn remove_iroh_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("not a valid file"))?
            .to_str()
            .ok_or_else(|| anyhow!("invalid filename"))?;
        let name = format!("{IROH_FILE}.{name}");
        let path = path.with_file_name(name);
        info!("removing iroh file {}", path.display());
        let key = path_to_key(path).map_err(|_| anyhow!("invalid path"))?;
        self.doc.del(self.author, key).await?;

        Ok(())
    }

    async fn update_iroh_attrs(&self, path: impl AsRef<Path>, attrs: &Attrs) -> Result<()> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("not a valid file"))?
            .to_str()
            .ok_or_else(|| anyhow!("invalid filename"))?;
        let name = match attrs.ftype {
            FileType::File => format!("{IROH_FILE}.{name}"),
            FileType::Directory => format!("{IROH_DIR}.{name}"),
        };
        let path = path.with_file_name(name);
        info!("udpating iroh entry {}", path.display());
        let key = path_to_key(path).map_err(|_| anyhow!("invalid path"))?;
        let attrs_bytes = serde_json::ser::to_vec_pretty(attrs)?;
        self.doc.set_bytes(self.author, key, attrs_bytes).await?;

        Ok(())
    }
}

// For this demo file system we let the handle just be the file
// there is only 1 file. a.txt.
#[async_trait]
impl<C> NFSFileSystem for IrohFs<C>
where
    C: ServiceConnection<ProviderService>,
{
    fn root_dir(&self) -> fileid3 {
        self.rootdir
    }

    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let fs = self.fs.0.read().await;
        info!("write to {:?}", id);
        let attr = {
            let file = fs.get_by_id(id).ok_or_else(|| {
                error!("missing entry {}", id);
                nfsstat3::NFS3ERR_NOENT
            })?;
            if !file.is_file() {
                return Err(nfsstat3::NFS3ERR_NOENT);
            }
            let author = *file.contents.author();
            let path = fs.get_path_for_id(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            let key = path_to_key(&path)?;

            info!("writing to {:?} - {} bytes at {}", path, data.len(), offset,);

            // get the full content
            let mut bytes = if let Some(entry) = self
                .doc
                .get_exact(author, &key, true)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?
            {
                self.iroh
                    .blobs
                    .read_to_bytes(entry.content_hash())
                    .await
                    .map_err(|e| {
                        error!("failed to read {}: {:?}", entry.content_hash(), e);
                        nfsstat3::NFS3ERR_SERVERFAULT
                    })?
                    .to_vec()
            } else {
                Vec::new()
            };

            let start = offset as usize;
            let end = start + data.len();

            // resize buffer if needed
            if end > bytes.len() {
                bytes.resize(end, 0);
            }

            bytes[start..end].copy_from_slice(data);
            let fssize = bytes.len() as u64;

            // store back
            let _hash = self
                .doc
                .set_bytes(self.author, key.clone(), bytes)
                .await
                .map_err(|e| {
                    error!("failed to set bytes {:?}: {:?}", path, e);
                    nfsstat3::NFS3ERR_SERVERFAULT
                })?;
            info!(
                "written {} bytes at offset {}: final size: {}",
                data.len(),
                offset,
                fssize
            );
            let mut attr = file.attr.clone();
            attr.size = fssize;
            attr.mtime = now();

            self.update_iroh_attrs(&path, &attr).await.map_err(|e| {
                error!("failed to update attrs {:?}: {:?}", path, e);
                nfsstat3::NFS3ERR_SERVERFAULT
            })?;

            attr
        };

        Ok(attr.into())
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        setattr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let fileid = {
            let fs = self.fs.0.read().await;
            let newid = self.next_id.fetch_add(1, Ordering::Relaxed) as fileid3;
            let name = safe_name(filename.as_ref());
            let path = fs
                .get_path_for_id(dirid)
                .ok_or_else(|| nfsstat3::NFS3ERR_NOENT)?
                .join(&name);
            info!("create {}: {:?} as {}", newid, name, path.display());
            let dir = fs.get_by_id(dirid).ok_or_else(|| {
                warn!("parentdir not found {}", dirid);
                nfsstat3::NFS3ERR_NOENT
            })?;
            if !dir.is_dir() {
                warn!("found file, expected directory");
                return Err(nfsstat3::NFS3ERR_NOTDIR);
            };

            // remove old
            self.remove_iroh_file(&path).await.ok(); // might not exist

            // Not writing to iroh, as we are not storing empty entries
            let file = FsEntry::new_file(&name, newid, dirid, 0, self.author, None);
            self.create_iroh_file(&path, &file.attr)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;

            newid
        };

        // TODO: optimize
        let attr = self.setattr(fileid, setattr).await?;

        Ok((attr.fileid, attr))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let fs = self.fs.0.read().await;
        let newid = self.next_id.fetch_add(1, Ordering::Relaxed) as fileid3;
        let name = safe_name(filename.as_ref());
        let path = fs
            .get_path_for_id(dirid)
            .ok_or_else(|| nfsstat3::NFS3ERR_NOENT)?
            .join(&name);
        info!(
            "create_exclusive {}: {:?} as {}",
            newid,
            name,
            path.display()
        );

        if fs.contains_by_path(&path) {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }

        let dir = fs.get_by_id(dirid).ok_or_else(|| {
            warn!("no parent dir found {}", dirid);
            nfsstat3::NFS3ERR_NOENT
        })?;
        if !dir.is_dir() {
            warn!("found file, expected directory");
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        };

        let file = FsEntry::new_file(&name, newid, dirid, 0, self.author, None);
        self.create_iroh_file(&path, &file.attr)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;

        Ok(newid)
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let fs = self.fs.0.read().await;
        let dir = fs.get_by_id(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if !dir.is_dir() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        // if looking for dir/. its the current directory
        if filename[..] == [b'.'] {
            return Ok(dirid);
        }
        // if looking for dir/.. its the parent directory
        if filename[..] == [b'.', b'.'] {
            return Ok(dir.parent);
        }

        let filename = safe_name(filename);
        for fileid in dir.contents.children() {
            if let Some(file) = fs.get_by_id(*fileid) {
                if file.attr.name == filename {
                    return Ok(*fileid);
                }
            }
        }
        Err(nfsstat3::NFS3ERR_NOENT)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        info!("getattr {:?}", id);
        let fs = self.fs.0.read().await;
        let entry = fs.get_by_id(id).ok_or_else(|| {
            error!("missing entry {}", id);
            nfsstat3::NFS3ERR_NOENT
        })?;
        debug!("got entry: {:?}", entry);

        // update attrs if needed
        let author = *entry.contents.author();
        let path = fs.get_path_for_id(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let key = path_to_key(&path)?;
        drop(fs);

        debug!("updating entry: {}: {}", id, path.display());
        let mut fs = self.fs.0.write().await;
        let fs_entry = fs.get_by_id_mut(id).ok_or_else(|| {
            error!("missing entry {}", id);
            nfsstat3::NFS3ERR_NOENT
        })?;

        if let Some(entry) = self
            .doc
            .get_exact(author, key, true)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?
        {
            fs_entry.attr.mtime = ts_to_chrono(entry.timestamp());
        }
        let attrs = fs_entry.attr.clone();

        debug!("got attrs {:?}", attrs);
        Ok(attrs.into())
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let fs = self.fs.0.read().await;

        let path = fs.get_path_for_id(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let key = path_to_key(&path)?;
        let entry = fs.get_by_id(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let mut attr = entry.attr.clone();

        info!("setattr {}:{}: {:?}", id, entry.attr.name, setattr,);

        match setattr.atime {
            nfs::set_atime::DONT_CHANGE => {}
            nfs::set_atime::SET_TO_CLIENT_TIME(c) => {
                attr.atime = nfstime_to_chrono(c);
            }
            nfs::set_atime::SET_TO_SERVER_TIME => {
                attr.atime = now();
            }
        };
        match setattr.mtime {
            nfs::set_mtime::DONT_CHANGE => {}
            nfs::set_mtime::SET_TO_CLIENT_TIME(c) => {
                attr.mtime = nfstime_to_chrono(c);
            }
            nfs::set_mtime::SET_TO_SERVER_TIME => {
                attr.mtime = now();
            }
        };
        match setattr.uid {
            nfs::set_uid3::uid(u) => {
                // TODO:
                // entry.attr.uid = u;
            }
            nfs::set_uid3::Void => {}
        }
        match setattr.gid {
            nfs::set_gid3::gid(u) => {
                // TODO:
                // entry.attr.gid = u;
            }
            nfs::set_gid3::Void => {}
        }
        match setattr.size {
            nfs::set_size3::size(s) => {
                attr.size = s;

                if let FsContents::File { author } = &entry.contents {
                    if s == 0 {
                        self.doc
                            .del(*author, key.clone())
                            .await
                            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;
                    } else {
                        let entry = self
                            .doc
                            .get_exact(*author, &key, true)
                            .await
                            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?
                            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

                        // get the full content
                        let mut bytes = self
                            .iroh
                            .blobs
                            .read_to_bytes(entry.content_hash())
                            .await
                            .map_err(|err| {
                                error!("read_to_bytes: {:?} {:?}", key, err);
                                nfsstat3::NFS3ERR_SERVERFAULT
                            })?
                            .to_vec();

                        bytes.resize(s as usize, 0);

                        // store back
                        self.doc
                            .set_bytes(self.author, key.clone(), bytes)
                            .await
                            .map_err(|err| {
                                error!("set_bytes: {:?} {:?}", key, err);
                                nfsstat3::NFS3ERR_SERVERFAULT
                            })?;
                    }
                };
            }
            nfs::set_size3::Void => {}
        }

        self.update_iroh_attrs(&path, &attr)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;

        Ok(attr.into())
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        info!("reading {}: {} bytes at {}", id, count, offset);
        let fs = self.fs.0.read().await;
        let file = fs.get_by_id(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let path = fs.get_path_for_id(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let key = path_to_key(&path)?;

        if !file.is_file() {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }

        let mut start = offset as usize;
        let mut end = offset as usize + count as usize;

        debug!("reading from {}: {}", path.display(), safe_name(&key),);
        let entry = self
            .doc
            .get_exact(*file.contents.author(), key, true)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        // TODO: partial reads
        let bytes = self
            .iroh
            .blobs
            .read_to_bytes(entry.content_hash())
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;
        let eof = end >= bytes.len();
        if start >= bytes.len() {
            start = bytes.len();
        }
        if end > bytes.len() {
            end = bytes.len();
        }

        Ok((bytes[start..end].to_vec(), eof))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        info!("readdir {}: {}-{}", dirid, start_after, max_entries);

        let fs = self.fs.0.read().await;
        let dir = fs.get_by_id(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if !dir.is_dir() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        let content = dir.contents.children();
        let mut ret = ReadDirResult {
            entries: Vec::new(),
            end: false,
        };
        let mut start_index = 0;
        if start_after > 0 {
            if let Some(pos) = content.iter().position(|&r| r == start_after) {
                start_index = pos + 1;
            } else {
                return Err(nfsstat3::NFS3ERR_BAD_COOKIE);
            }
        }
        let remaining_length = content.len() - start_index;

        for i in content[start_index..].iter() {
            let entry = fs.get_by_id(*i).ok_or(nfsstat3::NFS3ERR_IO)?;
            debug!("read entry {}: {}", i, entry.attr.name);
            ret.entries.push(DirEntry {
                fileid: *i,
                name: entry.attr.name.as_bytes().into(),
                attr: entry.attr.clone().into(),
            });
            if ret.entries.len() >= max_entries {
                break;
            }
        }
        if ret.entries.len() == remaining_length {
            ret.end = true;
        }
        Ok(ret)
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        info!("remove {:?} from {}", std::str::from_utf8(filename), dirid);
        let fs = self.fs.0.read().await;
        let path = fs
            .get_path_for_file_in_dir(dirid, filename)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let key = path_to_key(&path)?;
        let author = *fs
            .get_by_path(&path)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?
            .contents
            .author();

        // Remove from doc
        self.doc.del(author, key.clone()).await.map_err(|err| {
            error!("delete {:?}: {:?}", key, err);
            nfsstat3::NFS3ERR_SERVERFAULT
        })?;

        self.remove_iroh_file(&path)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;

        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        info!(
            "rename {:?} to {:?}",
            std::str::from_utf8(from_filename),
            std::str::from_utf8(to_filename)
        );
        let fs = self.fs.0.read().await;

        if !fs.contains_by_id(from_dirid) {
            warn!("missing from: {}", from_dirid);
            return Err(nfsstat3::NFS3ERR_NOENT);
        }

        if !fs.contains_by_id(to_dirid) {
            warn!("missing to: {}", to_dirid);
            return Err(nfsstat3::NFS3ERR_NOENT);
        }

        // read entry
        let old_path = fs
            .get_path_for_file_in_dir(from_dirid, from_filename)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let old_key = path_to_key(&old_path)?;

        let file = fs.get_by_path(&old_path).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let FsContents::File { author } = &file.contents else {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        };

        let new_path = fs
            .get_path_for_file_in_dir(to_dirid, to_filename)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let new_key = path_to_key(&new_path)?;

        // move actual files
        if let Some(entry) = self
            .doc
            .get_exact(*author, old_key.clone(), true)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?
        {
            self.doc
                .set_hash(
                    self.author,
                    new_key,
                    entry.content_hash(),
                    entry.content_len(),
                )
                .await
                .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;

            // delete old entry
            self.doc
                .del(self.author, old_key)
                .await
                .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;
        } else {
            // assume empty entry, ignore
        }

        // move iroh files
        self.remove_iroh_file(&old_path)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;

        self.create_iroh_file(&new_path, &file.attr)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;

        Ok(())
    }

    async fn mkdir(
        &self,
        parent_dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        info!("mkdir {:?}", std::str::from_utf8(dirname));
        let fs = self.fs.0.read().await;

        let parent = fs.get_by_id(parent_dirid).ok_or_else(|| {
            error!("unknown dir {}", parent_dirid);
            nfsstat3::NFS3ERR_NOTDIR
        })?;

        if !parent.is_dir() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        let newid = self.next_id.fetch_add(1, Ordering::Relaxed) as fileid3;
        let name = format!(
            "{}/",
            std::str::from_utf8(dirname.as_ref()).map_err(|_| nfsstat3::NFS3ERR_NAMETOOLONG)?
        );
        let path = fs
            .get_path_for_file_in_dir(parent_dirid, &name)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let dir = FsEntry::new_dir(&name, newid, parent_dirid, self.author, Vec::new());

        // write metadata to document
        self.create_iroh_dir(path, &dir.attr)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)?;

        Ok((newid, dir.attr.clone().into()))
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        error!("missing readlink");
        return Err(nfsstat3::NFS3ERR_NOTSUPP);
    }
}
