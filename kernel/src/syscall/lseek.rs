// SPDX-License-Identifier: MPL-2.0

use super::SyscallReturn;
use crate::{
    fs::file::file_table::{RawFileDesc, get_file_fast},
    prelude::*,
};

pub fn sys_lseek(
    raw_fd: RawFileDesc,
    offset: isize,
    whence: u32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    debug!(
        "raw_fd = {}, offset = {}, whence = {}",
        raw_fd, offset, whence
    );

    let mut file_table = ctx.thread_local.borrow_file_table_mut();
    let file = get_file_fast!(&mut file_table, raw_fd.try_into()?);

    let new_offset = match SeekType::try_from(whence)? {
        SeekType::SEEK_SET => {
            file.seek(core::convert::TryInto::try_into(offset.cast_unsigned())?)?
        }
        SeekType::SEEK_CUR => file.seek(core::convert::TryInto::try_into(offset)?)?,
        SeekType::SEEK_END => file.seek(core::convert::TryInto::try_into(offset)?)?,
        SeekType::SEEK_DATA => {
            let offset = offset.cast_unsigned();
            file.seek_data(offset)?
        }
        SeekType::SEEK_HOLE => {
            let offset = offset.cast_unsigned();
            file.seek_hole(offset)?
        }
    };

    Ok(SyscallReturn::Return(new_offset as _))
}

// Reference: <https://elixir.bootlin.com/linux/v6.17.7/source/include/uapi/linux/fs.h#L52>
#[expect(non_camel_case_types)]
#[repr(u32)]
#[derive(Clone, Copy, Debug, TryFromInt)]
enum SeekType {
    SEEK_SET = 0,
    SEEK_CUR = 1,
    SEEK_END = 2,
    SEEK_DATA = 3,
    SEEK_HOLE = 4,
}
