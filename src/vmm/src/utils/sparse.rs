// Copyright 2026 Superserve AI. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Walking a sparse file's allocated extents.

use std::fs::File;
use std::os::fd::AsRawFd;

/// Call `f(start, end)` for every allocated data extent of `file` within its
/// first `size` bytes, in ascending order. Ends are exclusive; an extent that
/// runs to the end of the file ends at `size`.
pub fn for_each_data_extent(
    file: &File,
    size: u64,
    mut f: impl FnMut(u64, u64),
) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    let mut off: libc::off_t = 0;
    while (off as u64) < size {
        // SAFETY: fd is a valid open file; SEEK_DATA returns the next data offset
        // at or after `off`, or -1/ENXIO once no data remains.
        let data = unsafe { libc::lseek(fd, off, libc::SEEK_DATA) };
        if data < 0 {
            let err = std::io::Error::last_os_error();
            // ENXIO is the documented "no more data" signal; any other errno is a real
            // failure that must not be mistaken for a fully-scanned (sparse) file.
            if err.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            return Err(err);
        }
        // SAFETY: same fd; SEEK_HOLE returns the next hole at or after `data`,
        // or EOF if the extent runs to the end of the file. It has no "no more
        // holes" errno: any failure is real and must not be read as an extent
        // that runs to the end of the file.
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let end = (hole as u64).min(size);
        if (data as u64) < end {
            f(data as u64, end);
        }
        off = hole;
    }
    Ok(())
}

/// `FS_IOC_FIEMAP`: `_IOWR('f', 11, struct fiemap)`, a 32-byte argument.
const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
const FIEMAP_FLAG_SYNC: u32 = 0x1;
const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x800;
const FIEMAP_BATCH: u32 = 256;

#[repr(C)]
#[derive(Clone, Copy)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

#[repr(C)]
struct Fiemap {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
    fm_extents: [FiemapExtent; FIEMAP_BATCH as usize],
}

/// Adjacent extents merged into one run: a filesystem reports a run that is
/// physically fragmented as several extents, and a caller judging block
/// coverage must see the whole run.
struct Runs {
    pending: Option<(u64, u64)>,
}

impl Runs {
    fn push(&mut self, start: u64, end: u64, f: &mut impl FnMut(u64, u64)) {
        match self.pending {
            Some((s, e)) if e == start => self.pending = Some((s, end)),
            Some((s, e)) => {
                f(s, e);
                self.pending = Some((start, end));
            }
            None => self.pending = Some((start, end)),
        }
    }

    fn finish(self, f: &mut impl FnMut(u64, u64)) {
        if let Some((s, e)) = self.pending {
            f(s, e);
        }
    }
}

/// Call `f(start, end)` for every run of written data in `file` within its
/// first `size` bytes, in ascending order, from the filesystem's own extent
/// map with adjacent extents merged. Unwritten (preallocated) extents read
/// as zeros and are not data. Unlike `SEEK_DATA`, which a filesystem may
/// answer by calling every byte data, this ioctl either reports real
/// allocation or fails, so a caller can trust a hole to be a hole.
pub fn for_each_allocated_extent(
    file: &File,
    size: u64,
    mut f: impl FnMut(u64, u64),
) -> std::io::Result<()> {
    let mut runs = Runs { pending: None };
    let mut start: u64 = 0;
    loop {
        let mut req = Fiemap {
            fm_start: start,
            fm_length: size.saturating_sub(start),
            fm_flags: FIEMAP_FLAG_SYNC,
            fm_mapped_extents: 0,
            fm_extent_count: FIEMAP_BATCH,
            fm_reserved: 0,
            fm_extents: [FiemapExtent {
                fe_logical: 0,
                fe_physical: 0,
                fe_length: 0,
                fe_reserved64: [0; 2],
                fe_flags: 0,
                fe_reserved: [0; 3],
            }; FIEMAP_BATCH as usize],
        };
        // SAFETY: `file` is open and `req` is a correctly sized, initialized
        // fiemap request with room for FIEMAP_BATCH extents.
        let ret = unsafe { vmm_sys_util::ioctl::ioctl_with_mut_ref(file, FS_IOC_FIEMAP, &mut req) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = req.fm_mapped_extents as usize;
        if n == 0 {
            break;
        }
        let mut last = false;
        let mut next = start;
        for e in &req.fm_extents[..n] {
            let s = e.fe_logical.max(start);
            let end = e.fe_logical.saturating_add(e.fe_length).min(size);
            if e.fe_flags & FIEMAP_EXTENT_UNWRITTEN == 0 && s < end {
                runs.push(s, end, &mut f);
            }
            next = next.max(e.fe_logical.saturating_add(e.fe_length));
            last |= e.fe_flags & FIEMAP_EXTENT_LAST != 0;
        }
        if last || next >= size || next <= start {
            break;
        }
        start = next;
    }
    runs.finish(&mut f);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    use super::*;

    #[test]
    fn adjacent_extents_merge_into_one_run() {
        let mut got = Vec::new();
        let mut f = |s, e| got.push((s, e));
        let mut runs = Runs { pending: None };
        runs.push(0, 1024, &mut f);
        runs.push(1024, 4096, &mut f);
        runs.push(8192, 12288, &mut f);
        runs.finish(&mut f);
        assert_eq!(got, vec![(0, 4096), (8192, 12288)]);
    }

    #[test]
    fn the_extent_map_reports_written_blocks_and_skips_holes_and_preallocation() {
        let tmp = vmm_sys_util::tempfile::TempFile::new().unwrap();
        let file = tmp.into_file();
        const BLOCK: u64 = 4096;
        file.set_len(8 * BLOCK).unwrap();
        file.write_all_at(&[0xAA; BLOCK as usize], BLOCK).unwrap();
        file.write_all_at(&[0xBB; BLOCK as usize], 5 * BLOCK)
            .unwrap();
        // Preallocated but never written: reads as zeros, so not data.
        // SAFETY: fallocate on a valid fd with an in-range offset and length.
        let ret = unsafe {
            libc::fallocate(
                file.as_raw_fd(),
                libc::FALLOC_FL_KEEP_SIZE,
                (3 * BLOCK) as i64,
                BLOCK as i64,
            )
        };
        assert_eq!(ret, 0, "{}", std::io::Error::last_os_error());
        file.sync_all().unwrap();

        let mut got = Vec::new();
        for_each_allocated_extent(&file, 8 * BLOCK, |s, e| got.push((s, e))).unwrap();
        // Filesystems may merge or split adjacent extents, so compare the
        // blocks covered rather than the extent boundaries.
        let mut blocks: Vec<u64> = got
            .iter()
            .flat_map(|&(s, e)| s / BLOCK..e.div_ceil(BLOCK))
            .collect();
        blocks.sort_unstable();
        blocks.dedup();
        assert_eq!(blocks, vec![1, 5]);
    }
}
