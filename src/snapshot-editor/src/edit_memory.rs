// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use clap::Subcommand;
use vmm::utils::u64_to_usize;
use vmm_sys_util::seek_hole::SeekHole;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum EditMemoryError {
    /// Could not open memory file: {0}
    OpenMemoryFile(std::io::Error),
    /// Could not open diff file: {0}
    OpenDiffFile(std::io::Error),
    /// Failed to seek data in diff file: {0}
    SeekDataDiff(std::io::Error),
    /// Failed to seek hole in diff file: {0}
    SeekHoleDiff(std::io::Error),
    /// Failed to get metadata for diff file: {0}
    MetadataDiff(std::io::Error),
    /// Failed to seek in memory file: {0}
    SeekMemory(std::io::Error),
    /// Failed to send the file: {0}
    SendFile(std::io::Error),
    /// Could not read the diff's presence side-car: {0}
    Presence(vmm::uffd_internal::InternalUffdError),
    /// Failed to write zeros to memory file: {0}
    WriteZeros(std::io::Error),
    /// Failed to write the memory file's presence side-car: {0}
    WritePresence(std::io::Error),
}

#[derive(Debug, Subcommand)]
pub enum EditMemorySubCommand {
    /// Apply a diff snapshot on top of a base one
    Rebase {
        /// Path to the memory file.
        #[arg(short, long)]
        memory_path: PathBuf,
        /// Path to the diff file.
        #[arg(short, long)]
        diff_path: PathBuf,
    },
}

pub fn edit_memory_command(command: EditMemorySubCommand) -> Result<(), EditMemoryError> {
    match command {
        EditMemorySubCommand::Rebase {
            memory_path,
            diff_path,
        } => rebase(memory_path, diff_path)?,
    }
    Ok(())
}

fn rebase(memory_path: PathBuf, diff_path: PathBuf) -> Result<(), EditMemoryError> {
    let mut base_file = OpenOptions::new()
        .write(true)
        .open(&memory_path)
        .map_err(EditMemoryError::OpenMemoryFile)?;

    let mut diff_file = OpenOptions::new()
        .read(true)
        .open(&diff_path)
        .map_err(EditMemoryError::OpenDiffFile)?;

    // A presence side-car makes a file authoritative for exactly the pages it marks. The diff's
    // decides which pages overwrite the base, holes among them as zeros; the base's must then
    // gain every page applied here. Both are read and checked before the base is touched, so
    // a bad side-car changes nothing.
    let page_size = vmm::arch::host_page_size();
    let present = vmm::uffd_internal::present_pages(&diff_path, page_size)
        .map_err(EditMemoryError::Presence)?;
    let base_present = vmm::uffd_internal::present_pages(&memory_path, page_size)
        .map_err(EditMemoryError::Presence)?;
    let diff_len = diff_file
        .metadata()
        .map_err(EditMemoryError::MetadataDiff)?
        .len();
    let page = page_size as u64;
    let npages = diff_len.div_ceil(page);
    let mut applied = vec![0u64; u64_to_usize(npages.div_ceil(64))];

    let base_side_car = vmm::uffd_internal::presence_sidecar_path(&memory_path);
    if base_present.is_some() {
        // Torn until the merged bitmap is written, so an interrupted merge is refused later.
        vmm::uffd_internal::write_file_durable(&base_side_car, &[])
            .map_err(EditMemoryError::WritePresence)?;
    }

    match present {
        None => apply_range(
            &mut base_file,
            &mut diff_file,
            0,
            diff_len,
            false,
            page,
            &mut applied,
        )?,
        Some(present) => {
            let is_set = |p: u64| present[u64_to_usize(p / 64)] & (1 << (p % 64)) != 0;
            let mut p = 0u64;
            while p < npages {
                if !is_set(p) {
                    p += 1;
                    continue;
                }
                let start = p;
                while p < npages && is_set(p) {
                    p += 1;
                }
                apply_range(
                    &mut base_file,
                    &mut diff_file,
                    start * page,
                    (p * page).min(diff_len),
                    true,
                    page,
                    &mut applied,
                )?;
            }
        }
    }

    if let Some(mut bits) = base_present {
        for (word, add) in bits.iter_mut().zip(&applied) {
            *word |= add;
        }
        base_file
            .sync_all()
            .map_err(EditMemoryError::WritePresence)?;
        let base_len = base_file
            .metadata()
            .map_err(EditMemoryError::WritePresence)?
            .len();
        vmm::uffd_internal::write_presence_bitmap(
            &memory_path,
            page_size,
            u64_to_usize(base_len.div_ceil(page)),
            &bits,
        )
        .map_err(EditMemoryError::WritePresence)?;
    }
    Ok(())
}

/// Copies the data extents of `diff_file` within `[start, end)` onto `base_file`, and writes
/// zeros over the holes there when `zero_holes` is set. Every page written is set in `applied`.
fn apply_range(
    base_file: &mut std::fs::File,
    diff_file: &mut std::fs::File,
    start: u64,
    end: u64,
    zero_holes: bool,
    page: u64,
    applied: &mut [u64],
) -> Result<(), EditMemoryError> {
    use std::os::unix::fs::FileExt;

    let mut mark = |from: u64, to: u64| {
        for p in from / page..to.div_ceil(page) {
            if let Some(word) = applied.get_mut(u64_to_usize(p / 64)) {
                *word |= 1 << (p % 64);
            }
        }
    };
    let zeros = vec![0u8; if zero_holes { 1 << 20 } else { 0 }];
    let mut cursor = start;
    while cursor < end {
        let data = diff_file
            .seek_data(cursor)
            .map_err(EditMemoryError::SeekDataDiff)?
            .unwrap_or(end)
            .min(end);
        if zero_holes && cursor < data {
            mark(cursor, data);
            while cursor < data {
                let n = (data - cursor).min(zeros.len() as u64);
                base_file
                    .write_all_at(&zeros[..u64_to_usize(n)], cursor)
                    .map_err(EditMemoryError::WriteZeros)?;
                cursor += n;
            }
        }
        cursor = data;
        if cursor >= end {
            break;
        }
        let block_end = diff_file
            .seek_hole(cursor)
            .map_err(EditMemoryError::SeekHoleDiff)?
            .unwrap_or(end)
            .min(end);
        mark(cursor, block_end);
        while cursor < block_end {
            base_file
                .seek(SeekFrom::Start(cursor))
                .map_err(EditMemoryError::SeekMemory)?;

            // SAFETY: Safe because the parameters are valid.
            let num_transferred_bytes = unsafe {
                libc::sendfile64(
                    base_file.as_raw_fd(),
                    diff_file.as_raw_fd(),
                    (&mut cursor as *mut u64).cast::<i64>(),
                    u64_to_usize(block_end.saturating_sub(cursor)),
                )
            };
            if num_transferred_bytes < 0 {
                return Err(EditMemoryError::SendFile(std::io::Error::last_os_error()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::FileExt;

    use vmm_sys_util::{rand, tempfile};

    use super::*;

    fn check_file_content(file: &File, expected_content: &[u8]) {
        assert_eq!(
            file.metadata().unwrap().len(),
            expected_content.len() as u64
        );
        let mut buf = vec![0u8; expected_content.len()];
        file.read_exact_at(buf.as_mut_slice(), 0).unwrap();
        assert_eq!(&buf, expected_content);
    }

    #[test]
    fn test_rebase_empty_files() {
        let base = tempfile::TempFile::new().unwrap();
        let diff = tempfile::TempFile::new().unwrap();

        let base_file = base.as_file();

        let base_path = base.as_path().to_path_buf();
        let diff_path = diff.as_path().to_path_buf();

        // Empty files
        rebase(base_path, diff_path).unwrap();
        assert_eq!(base_file.metadata().unwrap().len(), 0);
    }

    #[test]
    fn test_rebase_empty_diff() {
        let base = tempfile::TempFile::new().unwrap();
        let diff = tempfile::TempFile::new().unwrap();

        let mut base_file = base.as_file();
        let diff_file = diff.as_file();

        let base_path = base.as_path().to_path_buf();
        let diff_path = diff.as_path().to_path_buf();

        let initial_base_file_content = rand::rand_bytes(50000);
        base_file.write_all(&initial_base_file_content).unwrap();

        // Diff file that has only holes
        diff_file
            .set_len(initial_base_file_content.len() as u64)
            .unwrap();
        rebase(base_path, diff_path).unwrap();
        check_file_content(base_file, &initial_base_file_content);
    }

    #[test]
    fn test_rebase_full_diff() {
        let base = tempfile::TempFile::new().unwrap();
        let diff = tempfile::TempFile::new().unwrap();

        let base_file = base.as_file();
        let mut diff_file = diff.as_file();

        let base_path = base.as_path().to_path_buf();
        let diff_path = diff.as_path().to_path_buf();

        // Diff file that has only data
        let diff_data = rand::rand_bytes(50000);
        diff_file.write_all(&diff_data).unwrap();
        rebase(base_path, diff_path).unwrap();
        check_file_content(base_file, &diff_data);
    }

    #[test]
    fn test_rebase_zeroes_present_holes() {
        let base = tempfile::TempFile::new().unwrap();
        let diff = tempfile::TempFile::new().unwrap();
        let ps = vmm::arch::host_page_size();

        // The base holds data in all three pages; the diff writes page 0 and leaves pages 1
        // and 2 as holes, but its side-car marks page 1 present: page 1 reads as zeros.
        base.as_file().write_all(&vec![0xAA; 3 * ps]).unwrap();
        diff.as_file().set_len(3 * ps as u64).unwrap();
        diff.as_file().write_all_at(&vec![0xBB; ps], 0).unwrap();
        vmm::uffd_internal::write_presence_bitmap(diff.as_path(), ps, 3, &[0b011]).unwrap();

        rebase(base.as_path().to_path_buf(), diff.as_path().to_path_buf()).unwrap();
        let mut expected = vec![0xBB; ps];
        expected.extend(vec![0u8; ps]);
        expected.extend(vec![0xAA; ps]);
        check_file_content(base.as_file(), &expected);
        std::fs::remove_file(vmm::uffd_internal::presence_sidecar_path(diff.as_path())).unwrap();
    }

    #[test]
    fn test_rebase_bad_side_car_leaves_base() {
        let base = tempfile::TempFile::new().unwrap();
        let diff = tempfile::TempFile::new().unwrap();
        let ps = vmm::arch::host_page_size();

        base.as_file().write_all(&vec![0xAA; ps]).unwrap();
        diff.as_file().write_all(&vec![0xBB; ps]).unwrap();
        // An empty side-car is what an interrupted save leaves behind.
        let side_car = vmm::uffd_internal::presence_sidecar_path(diff.as_path());
        std::fs::write(&side_car, []).unwrap();

        rebase(base.as_path().to_path_buf(), diff.as_path().to_path_buf()).unwrap_err();
        check_file_content(base.as_file(), &vec![0xAA; ps]);
        std::fs::remove_file(side_car).unwrap();
    }

    #[test]
    fn test_rebase_skips_absent_pages() {
        let base = tempfile::TempFile::new().unwrap();
        let diff = tempfile::TempFile::new().unwrap();
        let ps = vmm::arch::host_page_size();

        // A transfer turned the diff's hole at page 1 into written zeros; the side-car still
        // marks it absent, so the base keeps its data there.
        base.as_file().write_all(&vec![0xAA; 2 * ps]).unwrap();
        diff.as_file().write_all(&vec![0xBB; ps]).unwrap();
        diff.as_file().write_all(&vec![0u8; ps]).unwrap();
        vmm::uffd_internal::write_presence_bitmap(diff.as_path(), ps, 2, &[0b01]).unwrap();

        rebase(base.as_path().to_path_buf(), diff.as_path().to_path_buf()).unwrap();
        let mut expected = vec![0xBB; ps];
        expected.extend(vec![0xAA; ps]);
        check_file_content(base.as_file(), &expected);
        std::fs::remove_file(vmm::uffd_internal::presence_sidecar_path(diff.as_path())).unwrap();
    }

    #[test]
    fn test_chained_rebase_keeps_presence() {
        let template = tempfile::TempFile::new().unwrap();
        let older = tempfile::TempFile::new().unwrap();
        let newer = tempfile::TempFile::new().unwrap();
        let ps = vmm::arch::host_page_size();

        // The template holds data in both pages. The older diff holds nothing yet; the newer
        // one writes page 0 and leaves page 1 as a hole it marks present, so it reads as zeros.
        template.as_file().write_all(&vec![0xAA; 2 * ps]).unwrap();
        older.as_file().set_len(2 * ps as u64).unwrap();
        vmm::uffd_internal::write_presence_bitmap(older.as_path(), ps, 2, &[0b00]).unwrap();
        newer.as_file().set_len(2 * ps as u64).unwrap();
        newer.as_file().write_all_at(&vec![0xBB; ps], 0).unwrap();
        vmm::uffd_internal::write_presence_bitmap(newer.as_path(), ps, 2, &[0b11]).unwrap();

        rebase(older.as_path().to_path_buf(), newer.as_path().to_path_buf()).unwrap();
        assert_eq!(
            vmm::uffd_internal::present_pages(older.as_path(), ps).unwrap(),
            Some(vec![0b11])
        );
        rebase(
            template.as_path().to_path_buf(),
            older.as_path().to_path_buf(),
        )
        .unwrap();
        let mut expected = vec![0xBB; ps];
        expected.extend(vec![0u8; ps]);
        check_file_content(template.as_file(), &expected);
        for f in [&older, &newer] {
            std::fs::remove_file(vmm::uffd_internal::presence_sidecar_path(f.as_path())).unwrap();
        }
    }

    #[test]
    fn test_rebase() {
        // The filesystem punches holes only for blocks >= 4096.
        // It doesn't make sense to test for smaller ones.
        let block_sizes: &[usize] = &[4096, 8192];
        for &block_size in block_sizes {
            let mut expected_result = vec![];

            let base = tempfile::TempFile::new().unwrap();
            let diff = tempfile::TempFile::new().unwrap();

            let mut base_file = base.as_file();
            let mut diff_file = diff.as_file();

            let base_path = base.as_path().to_path_buf();
            let diff_path = diff.as_path().to_path_buf();

            // 1. Populated block both in base and diff file
            // block:     [ ]
            // diff:      [ ]
            // expected:  [d]
            let base_block = rand::rand_bytes(block_size);
            base_file.write_all(&base_block).unwrap();
            let diff_block = rand::rand_bytes(block_size);
            diff_file.write_all(&diff_block).unwrap();
            expected_result.extend(diff_block);

            // 2. Populated block in base file, hole in diff file
            // block:     [ ] [ ]
            // diff:      [ ] ___
            // expected:  [d] [b]
            let base_block = rand::rand_bytes(block_size);
            base_file.write_all(&base_block).unwrap();
            diff_file
                .seek(SeekFrom::Current(i64::try_from(block_size).unwrap()))
                .unwrap();
            expected_result.extend(base_block);

            // 3. Populated block in base file, zeroes block in diff file
            // block:     [ ] [ ] [ ]
            // diff:      [ ] ___ [0]
            // expected:  [d] [b] [d]
            let base_block = rand::rand_bytes(block_size);
            base_file.write_all(&base_block).unwrap();
            let diff_block = vec![0u8; block_size];
            diff_file.write_all(&diff_block).unwrap();
            expected_result.extend(diff_block);

            // Rebase and check the result
            rebase(base_path.clone(), diff_path.clone()).unwrap();
            check_file_content(base_file, &expected_result);

            // 4. The diff file is bigger
            // block:     [ ] [ ] [ ]
            // diff:      [ ] ___ [0] [ ]
            // expected:  [d] [b] [d] [d]
            let diff_block = rand::rand_bytes(block_size);
            diff_file.write_all(&diff_block).unwrap();
            expected_result.extend(diff_block);
            // Rebase and check the result
            rebase(base_path.clone(), diff_path.clone()).unwrap();
            check_file_content(base_file, &expected_result);

            // 5. The base file is bigger
            // block:     [ ] [ ] [ ] [ ] [ ]
            // diff:      [ ] ___ [0] [ ]
            // expected:  [d] [b] [d] [d] [b]
            let base_block = rand::rand_bytes(block_size);
            // Adding to the base file 2 times because
            // it is 1 block smaller then diff right now.
            base_file.write_all(&base_block).unwrap();
            base_file.write_all(&base_block).unwrap();
            expected_result.extend(base_block);
            // Rebase and check the result
            rebase(base_path, diff_path).unwrap();
            check_file_content(base_file, &expected_result);
        }
    }
}
