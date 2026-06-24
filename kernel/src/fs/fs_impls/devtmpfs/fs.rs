// SPDX-License-Identifier: MPL-2.0

use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use align_ext::AlignExt;
use aster_block::SECTOR_SIZE;
use aster_util::slot_vec::SlotVec;
use device_id::DeviceId;
use hashbrown::HashMap;
use ostd::{
    mm::VmIo,
    sync::{PreemptDisabled, RwLockWriteGuard},
};

use super::{DEVTMPFS_MAGIC, BLOCK_SIZE, ROOT_INO, NAME_MAX};
use crate::{
    device::{self, DeviceType},
    fs::{
        file::{AccessMode, InodeMode, InodeType, PerOpenFileOps, StatusFlags, mkmod},
        pipe::Pipe,
        pseudofs::AnonDeviceId,
        utils::{CStr256, DirentVisitor},
        vfs::{
            file_system::{FileSystem, FsEventSubscriberStats, SuperBlock},
            inode::{
                Extension, FileOps, HardLinkability, Inode, Metadata, MknodType,
                SymbolicLink,
            },
            path::{is_dot, is_dot_or_dotdot, is_dotdot},
            registry::{FsCreationCtx, FsProperties, FsType},
        },
    },
    prelude::*,
    process::{Gid, Uid},
    time::clocks::RealTimeCoarseClock,
    vm::page_cache::PageCache,
};

/// A kernel-managed devtmpfs filesystem.
pub struct DevTmpFs {
    name: &'static str,
    _anon_device_id: AnonDeviceId,
    /// The super block
    sb: SuperBlock,
    /// Root inode
    root: Arc<DevTmpFsInode>,
    /// An inode allocator
    inode_allocator: AtomicU64,
    /// FS event subscriber stats for this file system
    fs_event_subscriber_stats: FsEventSubscriberStats,
}

impl DevTmpFs {
    pub fn new() -> Arc<Self> {
        Self::new_internal("devtmpfs")
    }

    fn new_internal(name: &'static str) -> Arc<Self> {
        let anon_device_id = AnonDeviceId::acquire().expect("no device ID is available for devtmpfs");
        let sb = SuperBlock::new(DEVTMPFS_MAGIC, BLOCK_SIZE, NAME_MAX, anon_device_id.id());
        Self::new_internal_with_sb(name, anon_device_id, sb)
    }

    fn new_internal_with_sb(
        name: &'static str,
        anon_device_id: AnonDeviceId,
        sb: SuperBlock,
    ) -> Arc<Self> {
        let root_dev_id = anon_device_id.id();
        Arc::new_cyclic(move |weak_fs| Self {
            name,
            _anon_device_id: anon_device_id,
            sb,
            root: Arc::new_cyclic(|weak_root| DevTmpFsInode {
                inner: Inner::new_dir(weak_root.clone(), weak_root.clone()),
                metadata: SpinLock::new(InodeMeta::new_dir(
                    mkmod!(a+rx, u+w),
                    Uid::new_root(),
                    Gid::new_root(),
                )),
                ino: ROOT_INO,
                typ: InodeType::Dir,
                this: weak_root.clone(),
                fs: weak_fs.clone(),
                container_dev_id: root_dev_id,
                hard_linkability: HardLinkability::Linkable,
                extension: Extension::new(),
            }),
            inode_allocator: AtomicU64::new(ROOT_INO + 1),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
        })
    }

    fn alloc_id(&self) -> u64 {
        self.inode_allocator.fetch_add(1, Ordering::Relaxed)
    }
}

impl FileSystem for DevTmpFs {
    fn name(&self) -> &'static str {
        self.name
    }

    fn sync(&self) -> Result<()> {
        // Do nothing for a memory-based filesystem
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }

    fn sb(&self) -> SuperBlock {
        self.sb.clone()
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }
}

/// An inode of `DevTmpFs`.
struct DevTmpFsInode {
    /// Inode inner specifics
    inner: Inner,
    /// Inode metadata
    metadata: SpinLock<InodeMeta>,
    /// Inode number
    ino: u64,
    /// Type of the inode
    typ: InodeType,
    /// Reference to self
    this: Weak<DevTmpFsInode>,
    /// Reference to fs
    fs: Weak<DevTmpFs>,
    /// Device ID
    container_dev_id: DeviceId,
    /// Hard linkability
    hard_linkability: HardLinkability,
    /// Extensions
    extension: Extension,
}

/// Inode inner specifics
enum Inner {
    Dir(RwLock<DirEntry>),
    File(Mutex<PageCache>),
    SymLink(SpinLock<String>),
    BlockDevice(u64),
    CharDevice(u64),
    NamedPipe(Pipe),
}

impl Inner {
    pub(self) fn new_dir(this: Weak<DevTmpFsInode>, parent: Weak<DevTmpFsInode>) -> Self {
        Self::Dir(RwLock::new(DirEntry::new(this, parent)))
    }

    pub(self) fn new_file() -> Self {
        Self::File(Mutex::new(PageCache::new_anon(0).unwrap()))
    }

    pub(self) fn new_symlink() -> Self {
        Self::SymLink(SpinLock::new(String::from("")))
    }

    pub(self) fn new_block_device(dev_id: u64) -> Self {
        Self::BlockDevice(dev_id)
    }

    pub(self) fn new_char_device(dev_id: u64) -> Self {
        Self::CharDevice(dev_id)
    }

    pub(self) fn new_named_pipe() -> Self {
        Self::NamedPipe(Pipe::new())
    }

    fn as_direntry(&self) -> Option<&RwLock<DirEntry>> {
        match self {
            Self::Dir(dir_entry) => Some(dir_entry),
            _ => None,
        }
    }

    fn as_file(&self) -> Option<&Mutex<PageCache>> {
        match self {
            Self::File(page_cache) => Some(page_cache),
            _ => None,
        }
    }

    fn as_symlink(&self) -> Option<&SpinLock<String>> {
        match self {
            Self::SymLink(link) => Some(link),
            _ => None,
        }
    }

    fn device_id(&self) -> Option<u64> {
        match self {
            Self::BlockDevice(dev_id) | Self::CharDevice(dev_id) => Some(*dev_id),
            _ => None,
        }
    }

    fn device_type(&self) -> Option<DeviceType> {
        match self {
            Self::BlockDevice(_) => Some(DeviceType::Block),
            Self::CharDevice(_) => Some(DeviceType::Char),
            _ => None,
        }
    }

    fn open(
        &self,
        access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Option<Result<Box<dyn PerOpenFileOps>>> {
        match self {
            Self::BlockDevice(device_id) | Self::CharDevice(device_id) => {
                let Some(device_id) = DeviceId::from_encoded_u64(*device_id) else {
                    return Some(Err(Error::with_message(
                        Errno::ENODEV,
                        "the device ID is invalid",
                    )));
                };

                let device_type = self.device_type().unwrap();
                let Some(device) = device::lookup(device_type, device_id) else {
                    return Some(Err(Error::with_message(
                        Errno::ENODEV,
                        "the required device ID does not exist",
                    )));
                };

                Some(device.open())
            }
            Self::NamedPipe(pipe) => Some(pipe.open_named(access_mode, status_flags)),
            _ => None,
        }
    }
}

/// Inode metadata
#[derive(Clone, Copy, Debug)]
struct InodeMeta {
    size: usize,
    blocks: usize,
    atime: Duration,
    mtime: Duration,
    ctime: Duration,
    mode: InodeMode,
    nlinks: usize,
    uid: Uid,
    gid: Gid,
}

impl InodeMeta {
    pub fn new(mode: InodeMode, uid: Uid, gid: Gid) -> Self {
        let now = now();
        Self {
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            mode,
            nlinks: 1,
            uid,
            gid,
        }
    }

    pub fn new_dir(mode: InodeMode, uid: Uid, gid: Gid) -> Self {
        let now = now();
        Self {
            size: NUM_SPECIAL_ENTRIES,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            mode,
            nlinks: NUM_SPECIAL_ENTRIES,
            uid,
            gid,
        }
    }

    pub fn resize(&mut self, new_size: usize) {
        self.size = new_size;
        self.blocks = new_size.align_up(BLOCK_SIZE) / BLOCK_SIZE;
    }

    pub fn inc_size(&mut self) {
        self.size += 1;
        self.blocks = self.size.align_up(BLOCK_SIZE) / BLOCK_SIZE;
    }

    pub fn dec_size(&mut self) {
        debug_assert!(self.size > 0);
        self.size -= 1;
        self.blocks = self.size.align_up(BLOCK_SIZE) / BLOCK_SIZE;
    }

    pub fn nr_sectors_allocated(&self) -> usize {
        self.blocks
            .checked_mul(BLOCK_SIZE / SECTOR_SIZE)
            .expect("devtmpfs allocated sector count overflow")
    }

    pub fn set_atime(&mut self, time: Duration) {
        self.atime = time;
    }

    pub fn set_mtime(&mut self, time: Duration) {
        self.mtime = time;
    }

    pub fn set_ctime(&mut self, time: Duration) {
        self.ctime = time;
    }

    pub fn inc_nlinks(&mut self) {
        self.nlinks += 1;
    }

    pub fn dec_nlinks(&mut self) {
        debug_assert!(self.nlinks > 0);
        self.nlinks -= 1;
    }
}

/// Directory entry
struct DirEntry {
    children: SlotVec<(CStr256, Arc<DevTmpFsInode>)>,
    idx_map: HashMap<CStr256, usize>, // Accelerate lookups
    this: Weak<DevTmpFsInode>,
    parent: Weak<DevTmpFsInode>,
}

const NUM_SPECIAL_ENTRIES: usize = 2; // . and ..

impl DirEntry {
    fn new(this: Weak<DevTmpFsInode>, parent: Weak<DevTmpFsInode>) -> Self {
        Self {
            children: SlotVec::new(),
            idx_map: HashMap::new(),
            this,
            parent,
        }
    }

    fn set_parent(&mut self, parent: Weak<DevTmpFsInode>) {
        self.parent = parent;
    }

    fn contains_entry(&self, name: &str) -> bool {
        if is_dot_or_dotdot(name) {
            true
        } else {
            self.idx_map.contains_key(name.as_bytes())
        }
    }

    fn get_entry(&self, name: &str) -> Option<(usize, Arc<DevTmpFsInode>)> {
        if is_dot(name) {
            Some((0, self.this.upgrade().unwrap()))
        } else if is_dotdot(name) {
            Some((1, self.parent.upgrade().unwrap()))
        } else {
            let idx = *self.idx_map.get(name.as_bytes())?;
            let target_inode = self
                .children
                .get(idx)
                .map(|(name_cstr256, inode)| {
                    debug_assert_eq!(name, name_cstr256.as_str().unwrap());
                    inode.clone()
                })
                .unwrap();
            Some((idx + NUM_SPECIAL_ENTRIES, target_inode))
        }
    }

    fn append_entry(&mut self, name: &str, inode: Arc<DevTmpFsInode>) -> usize {
        let name = CStr256::from(name);
        let idx = self.children.put((name, inode));
        self.idx_map.insert(name, idx);
        idx
    }

    fn remove_entry(&mut self, idx: usize) -> Option<(CStr256, Arc<DevTmpFsInode>)> {
        assert!(idx >= NUM_SPECIAL_ENTRIES);
        let removed = self.children.remove(idx - NUM_SPECIAL_ENTRIES)?;
        self.idx_map.remove(&removed.0);
        Some(removed)
    }

    fn substitute_entry(
        &mut self,
        idx: usize,
        new_entry: (CStr256, Arc<DevTmpFsInode>),
    ) -> Option<(CStr256, Arc<DevTmpFsInode>)> {
        assert!(idx >= NUM_SPECIAL_ENTRIES);
        let new_name = new_entry.0;
        let idx_children = idx - NUM_SPECIAL_ENTRIES;

        let substitute = self.children.put_at(idx_children, new_entry)?;
        let removed = self.idx_map.remove(&substitute.0);
        debug_assert_eq!(removed.unwrap(), idx_children);
        self.idx_map.insert(new_name, idx_children);
        Some(substitute)
    }

    fn visit_entry(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        let mut iterate_idx = offset;

        // Handle . and ..
        if iterate_idx == 0 {
            let this_inode = self.this.upgrade().unwrap();
            visitor.visit(".", this_inode.ino, this_inode.typ, 0)?;
            iterate_idx += 1;
        }
        if iterate_idx == 1 {
            let parent_inode = self.parent.upgrade().unwrap();
            visitor.visit("..", parent_inode.ino, parent_inode.typ, 1)?;
            iterate_idx += 1;
        }

        // Handle normal children
        for (idx, (name, child)) in self.children.idxes_and_items() {
            let entry_idx = idx + NUM_SPECIAL_ENTRIES;
            if entry_idx < iterate_idx {
                continue;
            }
            visitor.visit(name.as_str().unwrap(), child.ino, child.typ, entry_idx)?;
            iterate_idx += 1;
        }

        Ok(iterate_idx - offset)
    }

    fn is_empty_children(&self) -> bool {
        self.children.is_empty()
    }
}

impl DevTmpFsInode {
    fn new_dir(
        fs: &Arc<DevTmpFs>,
        mode: InodeMode,
        uid: Uid,
        gid: Gid,
        parent: &Weak<DevTmpFsInode>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak_self| DevTmpFsInode {
            inner: Inner::new_dir(weak_self.clone(), parent.clone()),
            metadata: SpinLock::new(InodeMeta::new_dir(mode, uid, gid)),
            ino: fs.alloc_id(),
            typ: InodeType::Dir,
            this: weak_self.clone(),
            fs: Arc::downgrade(fs),
            container_dev_id: fs.sb.container_dev_id,
            hard_linkability: HardLinkability::Linkable,
            extension: Extension::new(),
        })
    }

    fn new_file(fs: &Arc<DevTmpFs>, mode: InodeMode, uid: Uid, gid: Gid) -> Arc<Self> {
        Arc::new_cyclic(|weak_self| DevTmpFsInode {
            inner: Inner::new_file(),
            metadata: SpinLock::new(InodeMeta::new(mode, uid, gid)),
            ino: fs.alloc_id(),
            typ: InodeType::File,
            this: weak_self.clone(),
            fs: Arc::downgrade(fs),
            container_dev_id: fs.sb.container_dev_id,
            hard_linkability: HardLinkability::Linkable,
            extension: Extension::new(),
        })
    }

    fn new_symlink(fs: &Arc<DevTmpFs>, mode: InodeMode, uid: Uid, gid: Gid) -> Arc<Self> {
        Arc::new_cyclic(|weak_self| DevTmpFsInode {
            inner: Inner::new_symlink(),
            metadata: SpinLock::new(InodeMeta::new(mode, uid, gid)),
            ino: fs.alloc_id(),
            typ: InodeType::SymLink,
            this: weak_self.clone(),
            fs: Arc::downgrade(fs),
            container_dev_id: fs.sb.container_dev_id,
            hard_linkability: HardLinkability::Linkable,
            extension: Extension::new(),
        })
    }

    fn new_device(
        fs: &Arc<DevTmpFs>,
        mode: InodeMode,
        uid: Uid,
        gid: Gid,
        dev_type: DeviceType,
        dev_id: u64,
    ) -> Arc<Self> {
        let inner = match dev_type {
            DeviceType::Block => Inner::new_block_device(dev_id),
            DeviceType::Char => Inner::new_char_device(dev_id),
        };

        Arc::new_cyclic(|weak_self| DevTmpFsInode {
            inner,
            metadata: SpinLock::new(InodeMeta::new(mode, uid, gid)),
            ino: fs.alloc_id(),
            typ: dev_type.into(),
            this: weak_self.clone(),
            fs: Arc::downgrade(fs),
            container_dev_id: fs.sb.container_dev_id,
            hard_linkability: HardLinkability::Linkable,
            extension: Extension::new(),
        })
    }

    fn find(&self, name: &str) -> Result<Arc<Self>> {
        if self.typ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "self is not a directory");
        }

        let (_, inode) = self
            .inner
            .as_direntry()
            .unwrap()
            .read()
            .get_entry(name)
            .ok_or(Error::new(Errno::ENOENT))?;
        Ok(inode)
    }
}

impl FileOps for DevTmpFsInode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        let read_len = match &self.inner {
            Inner::File(page_cache) => {
                let (offset, read_len) = {
                    let file_size = self.size();
                    let start = file_size.min(offset);
                    let end = file_size.min(offset + writer.avail());
                    (start, end - start)
                };
                page_cache.lock().read(offset, writer)?;
                read_len
            }
            _ => return_errno_with_message!(Errno::EISDIR, "read is not supported"),
        };

        if self.typ == InodeType::File {
            self.set_atime(now());
        }
        Ok(read_len)
    }

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        let written_len = match self.typ {
            InodeType::File => {
                let now = now();

                let page_cache = self.inner.as_file().unwrap().lock();

                let mut inode_meta = self.metadata.lock();
                let file_size = inode_meta.size;
                let write_len = reader.remain();
                let new_size = offset + write_len;
                let should_expand_size = new_size > file_size;
                let new_size_aligned = new_size.align_up(BLOCK_SIZE);
                inode_meta.set_mtime(now);
                inode_meta.set_ctime(now);
                if should_expand_size {
                    inode_meta.size = new_size;
                    inode_meta.blocks = new_size_aligned / BLOCK_SIZE;
                }
                drop(inode_meta);

                if should_expand_size {
                    page_cache.resize(new_size_aligned, file_size)?;
                }
                page_cache.write(offset, reader)?;

                write_len
            }
            _ => return_errno_with_message!(Errno::EISDIR, "write is not supported"),
        };
        Ok(written_len)
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        if self.typ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "self is not a directory");
        }

        let cnt = self
            .inner
            .as_direntry()
            .unwrap()
            .read()
            .visit_entry(offset, visitor)?;

        self.set_atime(now());

        Ok(cnt)
    }
}

impl Inode for DevTmpFsInode {
    fn page_cache(&self) -> Option<PageCache> {
        self.inner
            .as_file()
            .map(|page_cache| page_cache.lock().clone())
    }

    fn size(&self) -> usize {
        self.metadata.lock().size
    }

    fn resize(&self, new_size: usize) -> Result<()> {
        if self.typ == InodeType::Dir {
            return_errno_with_message!(Errno::EISDIR, "the inode is a directory");
        }
        if self.typ != InodeType::File {
            return_errno_with_message!(Errno::EINVAL, "the inode is not a regular file");
        }

        let page_cache = self.inner.as_file().unwrap().lock();
        let mut inode_meta = self.metadata.lock();
        let file_size = inode_meta.size;
        if file_size == new_size {
            return Ok(());
        }
        let now = now();
        inode_meta.set_mtime(now);
        inode_meta.set_ctime(now);

        if new_size > file_size {
            inode_meta.resize(new_size);
            drop(inode_meta);
            page_cache.resize(new_size, file_size)?;
        } else {
            drop(inode_meta);
            page_cache.resize(new_size, file_size)?;
            self.metadata.lock().resize(new_size);
        }

        Ok(())
    }

    fn atime(&self) -> Duration {
        self.metadata.lock().atime
    }

    fn set_atime(&self, time: Duration) {
        self.metadata.lock().set_atime(time);
    }

    fn mtime(&self) -> Duration {
        self.metadata.lock().mtime
    }

    fn set_mtime(&self, time: Duration) {
        self.metadata.lock().set_mtime(time);
    }

    fn ctime(&self) -> Duration {
        self.metadata.lock().ctime
    }

    fn set_ctime(&self, time: Duration) {
        self.metadata.lock().set_ctime(time);
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn type_(&self) -> InodeType {
        self.typ
    }

    fn mode(&self) -> Result<InodeMode> {
        Ok(self.metadata.lock().mode)
    }

    fn set_mode(&self, mode: InodeMode) -> Result<()> {
        let mut inode_meta = self.metadata.lock();
        inode_meta.mode = mode;
        inode_meta.set_ctime(now());
        Ok(())
    }

    fn owner(&self) -> Result<Uid> {
        Ok(self.metadata.lock().uid)
    }

    fn set_owner(&self, uid: Uid) -> Result<()> {
        let mut inode_meta = self.metadata.lock();
        inode_meta.uid = uid;
        inode_meta.set_ctime(now());
        Ok(())
    }

    fn group(&self) -> Result<Gid> {
        Ok(self.metadata.lock().gid)
    }

    fn set_group(&self, gid: Gid) -> Result<()> {
        let mut inode_meta = self.metadata.lock();
        inode_meta.gid = gid;
        inode_meta.set_ctime(now());
        Ok(())
    }

    fn mknod(&self, name: &str, mode: InodeMode, type_: MknodType) -> Result<Arc<dyn Inode>> {
        if name.len() > NAME_MAX {
            return_errno!(Errno::ENAMETOOLONG);
        }
        if self.typ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "self is not a directory");
        }

        let self_dir = self.inner.as_direntry().unwrap().upread();
        if self_dir.contains_entry(name) {
            return_errno_with_message!(Errno::EEXIST, "entry exists");
        }

        let new_inode = match type_ {
            MknodType::CharDevice(dev_id) | MknodType::BlockDevice(dev_id) => {
                let dev_type = type_.device_type().unwrap();
                DevTmpFsInode::new_device(
                    &self.fs.upgrade().unwrap(),
                    mode,
                    Uid::new_root(),
                    Gid::new_root(),
                    dev_type,
                    dev_id,
                )
            }
            MknodType::NamedPipe => DevTmpFsInode::new_named_pipe(
                &self.fs.upgrade().unwrap(),
                mode,
                Uid::new_root(),
                Gid::new_root(),
            ),
        };

        let mut self_dir = self_dir.upgrade();
        self_dir.append_entry(name, new_inode.clone());
        drop(self_dir);

        self.metadata.lock().inc_size();
        Ok(new_inode)
    }

    fn open(
        &self,
        access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Option<Result<Box<dyn PerOpenFileOps>>> {
        self.inner.open(access_mode, status_flags)
    }

    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn Inode>> {
        if name.len() > NAME_MAX {
            return_errno!(Errno::ENAMETOOLONG);
        }
        if self.typ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "self is not a directory");
        }

        let self_dir = self.inner.as_direntry().unwrap().upread();
        if self_dir.contains_entry(name) {
            return_errno_with_message!(Errno::EEXIST, "entry exists");
        }

        let fs = self.fs.upgrade().unwrap();
        let new_inode = match type_ {
            InodeType::File => DevTmpFsInode::new_file(&fs, mode, Uid::new_root(), Gid::new_root()),
            InodeType::SymLink => {
                DevTmpFsInode::new_symlink(&fs, mode, Uid::new_root(), Gid::new_root())
            }
            InodeType::Dir => DevTmpFsInode::new_dir(&fs, mode, Uid::new_root(), Gid::new_root(), &self.this),
            _ => panic!("unsupported inode type"),
        };

        let mut self_dir = self_dir.upgrade();
        self_dir.append_entry(name, new_inode.clone());
        drop(self_dir);

        let now = now();
        let mut inode_meta = self.metadata.lock();
        inode_meta.set_mtime(now);
        inode_meta.set_ctime(now);
        inode_meta.inc_size();
        if type_ == InodeType::Dir {
            inode_meta.inc_nlinks();
        }

        Ok(new_inode)
    }

    fn link(&self, old: &Arc<dyn Inode>, name: &str) -> Result<()> {
        if !Arc::ptr_eq(&self.fs(), &old.fs()) {
            return_errno_with_message!(Errno::EXDEV, "not same fs");
        }
        if self.typ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "self is not a directory");
        }

        let old = old
            .downcast_ref::<DevTmpFsInode>()
            .ok_or(Error::new(Errno::EXDEV))?;
        if old.typ == InodeType::Dir {
            return_errno_with_message!(Errno::EPERM, "old is a directory");
        }
        if old.hard_linkability == HardLinkability::Unlinkable {
            return_errno_with_message!(Errno::ENOENT, "tmpfile is not linkable");
        }

        let mut self_dir = self.inner.as_direntry().unwrap().write();
        if self_dir.contains_entry(name) {
            return_errno_with_message!(Errno::EEXIST, "entry exists");
        }
        self_dir.append_entry(name, old.this.upgrade().unwrap());
        let now = now();

        let mut old_meta = old.metadata.lock();
        old_meta.inc_nlinks();
        old_meta.set_ctime(now);
        drop(old_meta);
        drop(self_dir);

        let mut self_meta = self.metadata.lock();
        self_meta.set_mtime(now);
        self_meta.set_ctime(now);
        self_meta.inc_size();

        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<()> {
        if is_dot_or_dotdot(name) {
            return_errno_with_message!(Errno::EISDIR, "unlink . or ..");
        }

        let target = self.find(name)?;
        if target.typ == InodeType::Dir {
            return_errno_with_message!(Errno::EISDIR, "unlink on directory");
        }

        let mut self_dir = self.inner.as_direntry().unwrap().write();
        let (idx, new_target) = self_dir.get_entry(name).ok_or(Error::new(Errno::ENOENT))?;
        if !Arc::ptr_eq(&new_target, &target) {
            return_errno!(Errno::ENOENT);
        }
        self_dir.remove_entry(idx);
        drop(self_dir);

        let now = now();
        let mut self_meta = self.metadata.lock();
        self_meta.dec_size();
        self_meta.set_mtime(now);
        self_meta.set_ctime(now);
        drop(self_meta);
        let mut target_meta = target.metadata.lock();
        target_meta.dec_nlinks();
        target_meta.set_ctime(now);

        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<()> {
        if is_dot(name) {
            return_errno_with_message!(Errno::EINVAL, "rmdir on .");
        }
        if is_dotdot(name) {
            return_errno_with_message!(Errno::ENOTEMPTY, "rmdir on ..");
        }

        let target = self.find(name)?;
        if target.typ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "rmdir on not a directory");
        }

        let (mut self_dir, target_dir) = write_lock_two_direntries_by_ino(
            (self.ino, self.inner.as_direntry().unwrap()),
            (target.ino, target.inner.as_direntry().unwrap()),
        );
        if !target_dir.is_empty_children() {
            return_errno_with_message!(Errno::ENOTEMPTY, "dir not empty");
        }
        let (idx, new_target) = self_dir.get_entry(name).ok_or(Error::new(Errno::ENOENT))?;
        if !Arc::ptr_eq(&new_target, &target) {
            return_errno!(Errno::ENOENT);
        }
        self_dir.remove_entry(idx);
        drop(self_dir);
        drop(target_dir);

        let now = now();
        let mut self_meta = self.metadata.lock();
        self_meta.dec_size();
        self_meta.dec_nlinks();
        self_meta.set_mtime(now);
        self_meta.set_ctime(now);
        drop(self_meta);
        let mut target_meta = target.metadata.lock();
        target_meta.dec_nlinks();
        target_meta.dec_nlinks();

        Ok(())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        let inode = self.find(name)?;
        Ok(inode as _)
    }

    fn rename(&self, old_name: &str, target: &Arc<dyn Inode>, new_name: &str) -> Result<()> {
        if is_dot_or_dotdot(old_name) || is_dot_or_dotdot(new_name) {
            return_errno_with_message!(Errno::EISDIR, "cannot rename . or ..");
        }

        let target = target
            .downcast_ref::<DevTmpFsInode>()
            .ok_or(Error::new(Errno::EXDEV))?;

        if !Arc::ptr_eq(&self.fs(), &target.fs()) {
            return_errno_with_message!(Errno::EXDEV, "not same fs");
        }
        if self.typ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "self is not a directory");
        }
        if target.typ != InodeType::Dir {
            return_errno_with_message!(Errno::ENOTDIR, "target is not a directory");
        }

        if self.ino == target.ino {
            let mut self_dir = self.inner.as_direntry().unwrap().write();
            let (src_idx, src_inode) = self_dir.get_entry(old_name).ok_or(Error::new(Errno::ENOENT))?;
            let _is_dir = src_inode.typ == InodeType::Dir;

            if let Some((dst_idx, dst_inode)) = self_dir.get_entry(new_name) {
                if src_inode.ino != dst_inode.ino {
                    if src_inode.typ == InodeType::Dir && !dst_inode.inner.as_direntry().unwrap().read().is_empty_children() {
                        return_errno_with_message!(Errno::ENOTEMPTY, "dir not empty");
                    }
                    self_dir.remove_entry(dst_idx);
                }
                self_dir.substitute_entry(src_idx, (CStr256::from(new_name), src_inode.clone()));
            } else {
                self_dir.substitute_entry(src_idx, (CStr256::from(new_name), src_inode.clone()));
            }

            let now = now();
            src_inode.set_ctime(now);
        } else {
            let (mut self_dir, mut target_dir) = write_lock_two_direntries_by_ino(
                (self.ino, self.inner.as_direntry().unwrap()),
                (target.ino, target.inner.as_direntry().unwrap()),
            );
            let (src_idx, src_inode) = self_dir.get_entry(old_name).ok_or(Error::new(Errno::ENOENT))?;
            let is_dir = src_inode.typ == InodeType::Dir;
            if Arc::ptr_eq(&src_inode, &target.this.upgrade().unwrap()) {
                return_errno!(Errno::EINVAL);
            }

            if let Some((dst_idx, dst_inode)) = target_dir.get_entry(new_name) {
                if src_inode.typ == InodeType::Dir && !dst_inode.inner.as_direntry().unwrap().read().is_empty_children() {
                    return_errno_with_message!(Errno::ENOTEMPTY, "dir not empty");
                }
                self_dir.remove_entry(src_idx);
                target_dir.remove_entry(dst_idx);
                target_dir.append_entry(new_name, src_inode.clone());
            } else {
                self_dir.remove_entry(src_idx);
                target_dir.append_entry(new_name, src_inode.clone());
            }

            if is_dir {
                src_inode
                    .inner
                    .as_direntry()
                    .unwrap()
                    .write()
                    .set_parent(target.this.clone());
            }
        }

        Ok(())
    }

    fn read_link(&self) -> Result<SymbolicLink> {
        if self.typ != InodeType::SymLink {
            return_errno_with_message!(Errno::EINVAL, "self is not a symlink");
        }

        let link = self.inner.as_symlink().unwrap().lock();
        Ok(SymbolicLink::Plain(link.clone()))
    }

    fn write_link(&self, target: &str) -> Result<()> {
        if self.typ != InodeType::SymLink {
            return_errno_with_message!(Errno::EINVAL, "self is not a symlink");
        }

        let mut link = self.inner.as_symlink().unwrap().lock();
        *link = String::from(target);
        self.metadata.lock().size = target.len();
        Ok(())
    }

    fn metadata(&self) -> Metadata {
        let rdev = self.inner.device_id().unwrap_or(0);
        let inode_metadata = self.metadata.lock();
        Metadata {
            ino: self.ino as _,
            size: inode_metadata.size,
            optimal_block_size: BLOCK_SIZE,
            nr_sectors_allocated: inode_metadata.nr_sectors_allocated(),
            last_access_at: inode_metadata.atime,
            last_modify_at: inode_metadata.mtime,
            last_meta_change_at: inode_metadata.ctime,
            type_: self.typ,
            mode: inode_metadata.mode,
            nr_hard_links: inode_metadata.nlinks,
            uid: inode_metadata.uid,
            gid: inode_metadata.gid,
            container_dev_id: self.container_dev_id,
            self_dev_id: if rdev == 0 {
                None
            } else {
                DeviceId::from_encoded_u64(rdev)
            },
        }
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Weak::upgrade(&self.fs).unwrap()
    }

    fn extension(&self) -> &Extension {
        &self.extension
    }
}

impl DevTmpFsInode {
    fn new_named_pipe(
        fs: &Arc<DevTmpFs>,
        mode: InodeMode,
        uid: Uid,
        gid: Gid,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak_self| DevTmpFsInode {
            inner: Inner::new_named_pipe(),
            metadata: SpinLock::new(InodeMeta::new(mode, uid, gid)),
            ino: fs.alloc_id(),
            typ: InodeType::NamedPipe,
            this: weak_self.clone(),
            fs: Arc::downgrade(fs),
            container_dev_id: fs.sb.container_dev_id,
            hard_linkability: HardLinkability::Linkable,
            extension: Extension::new(),
        })
    }
}

fn write_lock_two_direntries_by_ino<'a>(
    this: (u64, &'a RwLock<DirEntry>),
    other: (u64, &'a RwLock<DirEntry>),
) -> (
    RwLockWriteGuard<'a, DirEntry, PreemptDisabled>,
    RwLockWriteGuard<'a, DirEntry, PreemptDisabled>,
) {
    if this.0 < other.0 {
        (this.1.write(), other.1.write())
    } else {
        (other.1.write(), this.1.write())
    }
}

fn now() -> Duration {
    RealTimeCoarseClock::get().read_time()
}

pub(super) struct DevTmpFsType;

impl FsType for DevTmpFsType {
    fn name(&self) -> &'static str {
        "devtmpfs"
    }

    fn properties(&self) -> FsProperties {
        FsProperties::empty()
    }

    fn create(&self, _fs_creation_ctx: &FsCreationCtx) -> Result<Arc<dyn FileSystem>> {
        Ok(DevTmpFs::new())
    }

    fn sysnode(&self) -> Option<Arc<dyn aster_systree::SysNode>> {
        None
    }
}

// Devtmpfs singleton instance and API
use spin::Once;

static DEVTMPFS_INSTANCE: Once<Arc<DevTmpFs>> = Once::new();

/// Get or create the singleton devtmpfs instance
pub fn get_or_init_devtmpfs() -> Arc<DevTmpFs> {
    DEVTMPFS_INSTANCE.call_once(|| DevTmpFs::new()).clone()
}

/// Add a device node to devtmpfs
pub fn add_node(
    dev_type: DeviceType,
    dev_id: DeviceId,
    path: &str,
    mode: InodeMode,
) -> Result<()> {
    let fs = get_or_init_devtmpfs();
    let mut current_inode: Arc<dyn Inode> = fs.root_inode();

    let components: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let (parent_components, filename) = components.split_at(components.len() - 1);

    // Create parent directories if needed
    for component in parent_components {
        if component.is_empty() {
            continue;
        }
        current_inode = match current_inode.lookup(component) {
            Ok(inode) => inode,
            Err(_) => current_inode.create(component, InodeType::Dir, mkmod!(a+rx, u+w))?,
        };
    }

    let filename = filename.first().ok_or_else(|| Error::with_message(Errno::EINVAL, "empty filename"))?;

    let mknod_type = match dev_type {
        DeviceType::Char => MknodType::CharDevice(dev_id.as_encoded_u64()),
        DeviceType::Block => MknodType::BlockDevice(dev_id.as_encoded_u64()),
    };

    current_inode.mknod(filename, mode, mknod_type)?;
    Ok(())
}
