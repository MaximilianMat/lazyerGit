//! Object database access through gitoxide's `gix-odb` (packs, loose
//! objects, alternates).

use super::tree::{Kind, Objects, Oid};
use crate::api::Result;
use anyhow::{anyhow, Context};
use gix::objs::{Exists, Find, Write};
use std::path::Path;
use std::sync::Mutex;

pub struct GixOdb {
    id: usize,
    /// Cloned once per thread: a handle has its own caches and is not shared.
    proto: Mutex<gix::odb::Handle>,
}

thread_local! {
    static HANDLES: std::cell::RefCell<std::collections::HashMap<usize, gix::odb::Handle>> = Default::default();
}

impl GixOdb {
    pub fn open(common_dir: &Path) -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let handle = gix::odb::at(common_dir.join("objects"), gix::hash::Kind::Sha1).context("opening the object database")?;
        Ok(GixOdb { id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), proto: Mutex::new(handle) })
    }

    fn with<R>(&self, f: impl FnOnce(&gix::odb::Handle) -> R) -> R {
        HANDLES.with(|h| {
            let mut map = h.borrow_mut();
            let handle = map.entry(self.id).or_insert_with(|| self.proto.lock().unwrap().clone());
            f(handle)
        })
    }
}

fn to_gix(kind: Kind) -> gix::objs::Kind {
    match kind {
        Kind::Blob => gix::objs::Kind::Blob,
        Kind::Tree => gix::objs::Kind::Tree,
        Kind::Commit => gix::objs::Kind::Commit,
        Kind::Tag => gix::objs::Kind::Tag,
    }
}

impl Objects for GixOdb {
    fn read(&self, oid: &Oid) -> Result<(Kind, Vec<u8>)> {
        let mut buf = Vec::new();
        let id = gix::hash::ObjectId::from_bytes_or_panic(oid);
        let kind = self.with(|handle| -> Result<Kind> {
            let data = handle.try_find(&id, &mut buf).map_err(|e| anyhow!("reading object {id}: {e}"))?.with_context(|| format!("object {id} not found"))?;
            Ok(match data.kind {
                gix::objs::Kind::Blob => Kind::Blob,
                gix::objs::Kind::Tree => Kind::Tree,
                gix::objs::Kind::Commit => Kind::Commit,
                gix::objs::Kind::Tag => Kind::Tag,
            })
        })?;
        Ok((kind, buf))
    }

    fn write(&self, kind: Kind, data: &[u8]) -> Result<Oid> {
        let id = hash_object(kind, data);
        let gid = gix::hash::ObjectId::from_bytes_or_panic(&id);
        self.with(|handle| -> Result<()> {
            if !handle.exists(&gid) {
                handle.write_buf(to_gix(kind), data).map_err(|e| anyhow!("writing object {gid}: {e}"))?;
            }
            Ok(())
        })?;
        Ok(id)
    }
}

/// The id git gives to an object with this content.
pub fn hash_object(kind: Kind, data: &[u8]) -> Oid {
    let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, to_gix(kind), data).expect("hashing in memory cannot fail");
    let mut oid = [0u8; 20];
    oid.copy_from_slice(id.as_bytes());
    oid
}

pub fn sha1(data: &[u8]) -> Oid {
    let mut h = gix::hash::hasher(gix::hash::Kind::Sha1);
    h.update(data);
    let id = h.try_finalize().expect("hashing in memory cannot fail");
    let mut oid = [0u8; 20];
    oid.copy_from_slice(id.as_bytes());
    oid
}
