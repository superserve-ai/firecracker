// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

/// Module with helpers to read/write bytes into slices
pub mod byte_order;
/// Module with network related helpers
pub mod net;
/// Module with external libc functions
pub mod signal;
/// Module with state machine
pub mod sm;
/// Sparse-file extent walking.
pub mod sparse;

use std::fs::{File, OpenOptions};
use std::num::Wrapping;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use libc::O_NONBLOCK;

/// How many bits to left-shift by to convert MiB to bytes
const MIB_TO_BYTES_SHIFT: usize = 20;

/// Return the default page size of the platform, in bytes.
pub fn get_page_size() -> Result<usize, vmm_sys_util::errno::Error> {
    // SAFETY: Safe because the parameters are valid.
    match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        -1 => Err(vmm_sys_util::errno::Error::last()),
        ps => Ok(usize::try_from(ps).unwrap()),
    }
}

/// Safely converts a u64 value to a usize value.
/// This bypasses the Clippy lint check because we only support 64-bit platforms.
#[cfg(target_pointer_width = "64")]
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub const fn u64_to_usize(num: u64) -> usize {
    num as usize
}

/// Safely converts a usize value to a u64 value.
/// This bypasses the Clippy lint check because we only support 64-bit platforms.
#[cfg(target_pointer_width = "64")]
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub const fn usize_to_u64(num: usize) -> u64 {
    num as u64
}

/// Converts a usize into a wrapping u32.
#[inline]
pub const fn wrap_usize_to_u32(num: usize) -> Wrapping<u32> {
    Wrapping(((num as u64) & 0xFFFFFFFF) as u32)
}

/// Converts MiB to Bytes
pub const fn mib_to_bytes(mib: usize) -> usize {
    mib << MIB_TO_BYTES_SHIFT
}

/// Converts MiB to Bytes, widening to `u64` first so the result cannot wrap.
///
/// The largest input yields just under 4 PiB, which always fits in a `u64` byte count.
pub const fn u32_mib_to_bytes(mib: u32) -> u64 {
    (mib as u64) << MIB_TO_BYTES_SHIFT
}

/// Converts Bytes to MiB, truncating any remainder.
///
/// # Panics
///
/// Panics if the resulting MiB count does not fit in a `u32`. Callers must only pass byte
/// counts that originate from a `u32` MiB value, for which this is unreachable.
pub fn bytes_to_u32_mib(bytes: u64) -> u32 {
    u32::try_from(bytes >> MIB_TO_BYTES_SHIFT).expect("byte count does not originate from u32 MiB")
}

/// Converts Bytes to MiB, truncating any remainder
pub const fn bytes_to_mib(bytes: usize) -> usize {
    bytes >> MIB_TO_BYTES_SHIFT
}

/// Align address up to the aligment.
pub const fn align_up(addr: u64, align: u64) -> u64 {
    debug_assert!(align != 0);
    (addr + align - 1) & !(align - 1)
}

/// Align address down to the aligment.
pub const fn align_down(addr: u64, align: u64) -> u64 {
    debug_assert!(align != 0);
    addr & !(align - 1)
}

/// Create and open a file for both reading and writing to it with a O_NONBLOCK flag.
/// In case we open a FIFO, we need all READ, WRITE and O_NONBLOCK in order to not block the process
/// if nobody is consuming the message. Otherwise opening the FIFO with only WRITE and O_NONBLOCK
/// will fail with ENXIO if there is no readier already attached to it.
/// NOTE: writing to a pipe will start failing when reaching 64K of unconsumed content.
pub fn open_file_nonblock(path: &Path) -> Result<File, std::io::Error> {
    OpenOptions::new()
        .custom_flags(O_NONBLOCK)
        .create(true)
        .read(true)
        .write(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_u32_mib_to_bytes_does_not_wrap() {
        assert_eq!(u32_mib_to_bytes(0), 0);
        assert_eq!(u32_mib_to_bytes(1), 1 << 20);
        // The largest input is just under 4 PiB and stays within a u64.
        assert_eq!(u32_mib_to_bytes(u32::MAX), (u32::MAX as u64) << 20);
        // The same value shifted as a usize MiB count would have wrapped; widening first
        // means every u32 input is representable.
        assert!(u32_mib_to_bytes(u32::MAX) > u32::MAX.into());
    }

    #[test]
    fn test_bytes_to_u32_mib_round_trips() {
        for mib in [0, 1, 2, 128, 1024, u32::MAX] {
            assert_eq!(bytes_to_u32_mib(u32_mib_to_bytes(mib)), mib);
        }
        // Truncates a partial MiB, matching bytes_to_mib.
        assert_eq!(bytes_to_u32_mib((1 << 20) - 1), 0);
    }

    #[test]
    #[should_panic(expected = "byte count does not originate from u32 MiB")]
    fn test_bytes_to_u32_mib_panics_above_u32_mib() {
        bytes_to_u32_mib(u32_mib_to_bytes(u32::MAX) + (1 << 20));
    }
}
