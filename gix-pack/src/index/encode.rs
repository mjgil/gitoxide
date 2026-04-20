use std::cmp::Ordering;

pub(crate) const LARGE_OFFSET_THRESHOLD: u64 = 0x7fff_ffff;
pub(crate) const HIGH_BIT: u32 = 0x8000_0000;

pub(crate) fn fanout(iter: &mut dyn ExactSizeIterator<Item = u8>) -> [u32; 256] {
    let mut fan_out = [0u32; 256];
    let entries_len = iter.len() as u32;
    let mut iter = iter.enumerate();
    let mut idx_and_entry = iter.next();
    let mut upper_bound = 0;

    for (offset_be, byte) in fan_out.iter_mut().zip(0u8..=255) {
        *offset_be = match idx_and_entry.as_ref() {
            Some((_idx, first_byte)) => match first_byte.cmp(&byte) {
                Ordering::Less => unreachable!("ids should be ordered, and we make sure to keep ahead with them"),
                Ordering::Greater => upper_bound,
                Ordering::Equal => {
                    if byte == 255 {
                        entries_len
                    } else {
                        idx_and_entry = iter.find(|(_, first_byte)| *first_byte != byte);
                        upper_bound = idx_and_entry.as_ref().map_or(entries_len, |(idx, _)| *idx as u32);
                        upper_bound
                    }
                }
            },
            None => entries_len,
        };
    }

    fan_out
}

#[cfg(feature = "streaming-input")]
mod function {
    use std::{
        io::{self, Seek, SeekFrom, Write as _},
    };

    use gix_features::progress::{self, DynNestedProgress};

    use super::{HIGH_BIT, LARGE_OFFSET_THRESHOLD};
    use crate::index::{
        write::{external_sort::SortedIter, Error as WriteError},
        V2_SIGNATURE,
    };

    /// Wraps an `io::Write` and counts bytes written, for progress
    /// reporting at the end.
    struct Count<W> {
        bytes: u64,
        inner: W,
    }

    impl<W> Count<W> {
        fn new(inner: W) -> Self {
            Count { bytes: 0, inner }
        }
    }

    impl<W> io::Write for Count<W>
    where
        W: io::Write,
    {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let written = self.inner.write(buf)?;
            self.bytes += written as u64;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    /// Write a V2 pack index to `out` from a sorted-by-id entry
    /// stream.
    ///
    /// # Streaming shape (step 5.4b-2)
    ///
    /// Historically this function walked a materialized
    /// `Vec<IndexEntry>` four times (fanout, ids, crc32s, offsets).
    /// With external merge sort feeding the entries through a
    /// [`SortedIter`] that can only be consumed once, we now take a
    /// single streaming pass and stage each section to its own
    /// anonymous tempfile while walking:
    ///
    ///   * accumulate per-first-byte counts for the fanout table,
    ///   * append each 20-byte id to `tmp_ids`,
    ///   * append each 4-byte crc32 to `tmp_crc32s`,
    ///   * append each 4-byte offset32 (with [`HIGH_BIT`] set if the
    ///     true offset exceeds [`LARGE_OFFSET_THRESHOLD`]) to
    ///     `tmp_offsets32`, and in that case append the raw 8-byte
    ///     offset to `tmp_offsets64`.
    ///
    /// After the walk, the four staging tempfiles are rewound and
    /// streamed out in order via [`io::copy`]. Each tempfile is
    /// anonymous (created by [`tempfile::tempfile`] — on Linux an
    /// `O_TMPFILE` inode) and reclaimed by the kernel on drop.
    ///
    /// RAM during this function is O(1) in the number of entries:
    /// the 256-element fanout array, a handful of file handles, and
    /// the inner `BufWriter`'s 32 KiB buffer. For a 10M-object pack
    /// this replaces the pre-5.4b-2 ~320 MiB sorted Vec with
    /// ~280 MiB of scratch disk (20·4·4·8 bytes per entry across
    /// four tempfiles, plus the large-offsets tail).
    ///
    /// The produced bytes are unchanged — pack index v2 format is
    /// byte-for-byte identical to the old materialized-Vec path.
    /// The `write_to_stream` test in `gix-pack-tests/tests/pack/
    /// index.rs` compares output against canned V2 fixtures and is
    /// the load-bearing proof of that.
    pub(crate) fn write_to(
        out: &mut dyn io::Write,
        mut sorted: SortedIter,
        expected_entry_count: u32,
        pack_hash: &gix_hash::ObjectId,
        kind: crate::index::Version,
        progress: &mut dyn DynNestedProgress,
    ) -> Result<gix_hash::ObjectId, WriteError> {
        assert_eq!(kind, crate::index::Version::V2, "Can only write V2 packs right now");

        // Open the four per-section staging tempfiles. These die
        // with this function's stack frame even on early return:
        // `tempfile::tempfile()` returns a `File` whose inode has
        // already been unlinked, so Drop alone reclaims the blocks.
        let mut tmp_ids = tempfile::tempfile().map_err(gix_hash_io_err)?;
        let mut tmp_crc32s = tempfile::tempfile().map_err(gix_hash_io_err)?;
        let mut tmp_offsets32 = tempfile::tempfile().map_err(gix_hash_io_err)?;
        let mut tmp_offsets64 = tempfile::tempfile().map_err(gix_hash_io_err)?;

        // Buffered writers so each entry doesn't incur a syscall.
        // 32 KiB = 1024 ids, 8192 crc32s, 8192 offset32s, 4096
        // offset64s. Small enough that four of them fit in any
        // reasonable budget without needing MemoryBudget accounting.
        let mut buf_ids = io::BufWriter::with_capacity(32 * 1024, &mut tmp_ids);
        let mut buf_crc32s = io::BufWriter::with_capacity(32 * 1024, &mut tmp_crc32s);
        let mut buf_offsets32 = io::BufWriter::with_capacity(32 * 1024, &mut tmp_offsets32);
        let mut buf_offsets64 = io::BufWriter::with_capacity(32 * 1024, &mut tmp_offsets64);

        progress.init(Some(4), progress::steps());
        let start = std::time::Instant::now();
        let _info = progress.add_child_with_id(
            "staging sorted entries".into(),
            gix_features::progress::UNKNOWN,
        );

        // Running fanout[first_byte] counts; converted to cumulative
        // after the walk completes.
        let mut fan_out_counts = [0u32; 256];
        // Number of offsets that needed promotion to the 64-bit
        // tail. Used both as the u32 index we write into offsets32
        // (with HIGH_BIT) AND as the count of u64s in the tail.
        let mut offsets64_count: u32 = 0;
        let mut seen: u32 = 0;

        while let Some(entry) = sorted.try_next()? {
            assert!(
                seen < expected_entry_count,
                "sorted iterator yielded more entries ({}) than expected ({})",
                seen as u64 + 1,
                expected_entry_count
            );
            fan_out_counts[usize::from(entry.id.first_byte())] += 1;
            buf_ids.write_all(entry.id.as_slice()).map_err(gix_hash_io_err)?;
            buf_crc32s
                .write_all(&entry.crc32.to_be_bytes())
                .map_err(gix_hash_io_err)?;
            let offset32: u32 = if entry.offset > LARGE_OFFSET_THRESHOLD {
                assert!(
                    (offsets64_count as u64) < LARGE_OFFSET_THRESHOLD,
                    "Encoding breakdown — way too many 64bit offsets"
                );
                buf_offsets64
                    .write_all(&entry.offset.to_be_bytes())
                    .map_err(gix_hash_io_err)?;
                let v = offsets64_count | HIGH_BIT;
                offsets64_count += 1;
                v
            } else {
                entry.offset as u32
            };
            buf_offsets32
                .write_all(&offset32.to_be_bytes())
                .map_err(gix_hash_io_err)?;
            seen += 1;
        }
        assert_eq!(
            seen, expected_entry_count,
            "sorted iterator yielded {seen} entries, expected {expected_entry_count}"
        );

        // Convert fanout counts to cumulative (spec format).
        let mut fan_out_cumulative = [0u32; 256];
        let mut acc: u32 = 0;
        for (i, &count) in fan_out_counts.iter().enumerate() {
            acc = acc
                .checked_add(count)
                .expect("fanout cumulative sum fits in u32 because total entries fit in u32");
            fan_out_cumulative[i] = acc;
        }
        debug_assert_eq!(
            acc, expected_entry_count,
            "sum of fanout counts must equal total entries seen"
        );

        // Release the BufWriters so we can rewind the underlying
        // File handles. Flushing pushes any buffered bytes to disk.
        buf_ids.flush().map_err(gix_hash_io_err)?;
        buf_crc32s.flush().map_err(gix_hash_io_err)?;
        buf_offsets32.flush().map_err(gix_hash_io_err)?;
        buf_offsets64.flush().map_err(gix_hash_io_err)?;
        drop(buf_ids);
        drop(buf_crc32s);
        drop(buf_offsets32);
        drop(buf_offsets64);

        // Begin emitting the index file itself. Hash-writer wraps
        // the caller's `out` so the final index_hash is computed
        // over exactly the bytes we emit between now and the
        // pack_hash write (exclusive).
        let mut out = Count::new(std::io::BufWriter::with_capacity(
            8 * 4096,
            gix_hash::io::Write::new(out, kind.hash()),
        ));
        out.write_all(V2_SIGNATURE).map_err(gix_hash_io_err)?;
        out.write_all(&(kind as u32).to_be_bytes()).map_err(gix_hash_io_err)?;

        progress.inc();
        let _info = progress.add_child_with_id(
            "writing fan-out table".into(),
            gix_features::progress::UNKNOWN,
        );
        for value in fan_out_cumulative.iter() {
            out.write_all(&value.to_be_bytes()).map_err(gix_hash_io_err)?;
        }

        progress.inc();
        let _info = progress.add_child_with_id(
            "copying staged sections".into(),
            gix_features::progress::UNKNOWN,
        );
        // Rewind each staging file and stream it into out.
        // `io::copy` uses an 8 KiB stack buffer by default; negligible.
        tmp_ids.seek(SeekFrom::Start(0)).map_err(gix_hash_io_err)?;
        io::copy(&mut tmp_ids, &mut out).map_err(gix_hash_io_err)?;
        tmp_crc32s.seek(SeekFrom::Start(0)).map_err(gix_hash_io_err)?;
        io::copy(&mut tmp_crc32s, &mut out).map_err(gix_hash_io_err)?;
        tmp_offsets32
            .seek(SeekFrom::Start(0))
            .map_err(gix_hash_io_err)?;
        io::copy(&mut tmp_offsets32, &mut out).map_err(gix_hash_io_err)?;
        if offsets64_count > 0 {
            tmp_offsets64
                .seek(SeekFrom::Start(0))
                .map_err(gix_hash_io_err)?;
            io::copy(&mut tmp_offsets64, &mut out).map_err(gix_hash_io_err)?;
        }

        out.write_all(pack_hash.as_slice()).map_err(gix_hash_io_err)?;

        let bytes_written_without_trailer = out.bytes;
        let out = out.inner.into_inner().map_err(io::Error::from).map_err(gix_hash_io_err)?;
        let index_hash = out.hash.try_finalize().map_err(|e| WriteError::Io(e.into()))?;
        out.inner.write_all(index_hash.as_slice()).map_err(gix_hash_io_err)?;
        out.inner.flush().map_err(gix_hash_io_err)?;

        progress.inc();
        progress.show_throughput_with(
            start,
            (bytes_written_without_trailer + 20) as usize,
            progress::bytes().expect("unit always set"),
            progress::MessageLevel::Success,
        );

        Ok(index_hash)
    }

    /// Route a plain `std::io::Error` through the existing
    /// `gix_hash::io::Error::Io(#[from])` into
    /// [`WriteError::Io`]. Staging-tempfile errors are semantically
    /// "I/O during pack-index write" even though they don't involve
    /// hashing; routing them through the existing variant keeps the
    /// public error enum stable across 5.4b-2.
    fn gix_hash_io_err(e: io::Error) -> WriteError {
        WriteError::Io(e.into())
    }
}
#[cfg(feature = "streaming-input")]
pub(crate) use function::write_to;
