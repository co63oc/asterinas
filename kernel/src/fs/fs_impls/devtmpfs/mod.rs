// SPDX-License-Identifier: MPL-2.0

//! Kernel-managed devtmpfs (device temporary filesystem).

use fs::DevTmpFsType;
pub use fs::{add_node, get_or_init_devtmpfs};

mod fs;

pub(super) const DEVTMPFS_MAGIC: u64 = 0x1d1d1d1d; // TODO: Check Linux's devtmpfs magic
const BLOCK_SIZE: usize = 4096;
const ROOT_INO: u64 = 1;
const NAME_MAX: usize = 255;

pub(super) fn init() {
    crate::fs::vfs::registry::register(&DevTmpFsType).unwrap();
}
