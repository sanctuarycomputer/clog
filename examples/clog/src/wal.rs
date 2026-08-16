//! The append-only write-ahead log: clog's source of truth (spec §6.3,
//! INV-11). Every committed batch is appended as one length-prefixed,
//! CRC32-checked frame; reopening replays every frame in order. A torn or
//! corrupt tail is never applied: it is quarantined to `wal.corrupt` and
//! truncated from the log (recovery test R2), and replay never panics.
//!
//! **Frame format:** `[len: u32 LE][crc32(payload): u32 LE][payload]`,
//! where `payload` is `postcard::to_allocvec(&batch)` and the CRC covers
//! only the payload bytes.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::engine::Batch;
use crate::types::{ClogError, FsyncPolicy};

/// Header size in bytes: a `u32` length followed by a `u32` CRC32, both LE.
const HEADER_LEN: usize = 8;

/// A handle to the open WAL log file, ready to append further batches.
pub(crate) struct Wal {
    file: File,
    fsync: FsyncPolicy,
}

impl Wal {
    /// Appends `batch` as one `[len][crc32][payload]` frame, fsyncing per
    /// `self`'s policy afterwards (`OnCommit` calls `sync_data`; `Never`
    /// does not sync).
    pub(crate) fn append(&mut self, batch: &Batch) -> Result<(), ClogError> {
        let payload = postcard::to_allocvec(batch).map_err(|e| ClogError::Corrupt {
            detail: format!("wal encode: {e}"),
        })?;
        let len = u32::try_from(payload.len()).map_err(|_| ClogError::Corrupt {
            detail: "wal record too large".to_string(),
        })?;
        let crc = crc32fast::hash(&payload);

        let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&crc.to_le_bytes());
        frame.extend_from_slice(&payload);

        self.file.write_all(&frame)?;
        match self.fsync {
            FsyncPolicy::OnCommit => self.file.sync_data()?,
            FsyncPolicy::Never => {}
        }
        Ok(())
    }

    /// Fsyncs the log unconditionally, whatever the policy says.
    ///
    /// Called once by the writer thread on clean shutdown (spec §6.1), so
    /// that `FsyncPolicy::Never` still means "no fsync *per commit*" rather
    /// than "no fsync ever".
    pub(crate) fn sync(&mut self) -> Result<(), ClogError> {
        self.file.sync_data()?;
        Ok(())
    }
}

/// Opens (creating if needed) the WAL under `dir/wal/log`, replaying every
/// existing record in order. A torn or corrupt tail (short read, an
/// absurd length, a CRC mismatch, or — defensively — a postcard record
/// that fails to decode despite a matching CRC) is never applied: the
/// offending tail bytes are appended to `dir/wal/wal.corrupt` and the log
/// is truncated to the last good frame boundary before replay stops (R2).
/// Reopening afterwards is clean.
///
/// # Errors
///
/// A tail that is merely torn or corrupt is *not* an error (see above), but
/// a structurally impossible log is: `ClogError::Corrupt` if the replayed
/// revisions are not strictly increasing from 1. That cannot happen by
/// truncation — only by a bug or by two writers sharing one log — so it is
/// never silently repaired.
pub(crate) fn open_dir(dir: &Path, fsync: FsyncPolicy) -> Result<(Wal, Vec<Batch>), ClogError> {
    let wal_dir = dir.join("wal");
    fs::create_dir_all(&wal_dir)?;
    let log_path = wal_dir.join("log");

    let bytes = if log_path.exists() {
        fs::read(&log_path)?
    } else {
        Vec::new()
    };
    let (batches, good_len) = replay(&bytes)?;

    if good_len < bytes.len() {
        quarantine(&wal_dir, &bytes[good_len..])?;
        // Reopen for truncation: `append(true)` below would otherwise race
        // a second writable handle against this one.
        let trunc = OpenOptions::new().write(true).open(&log_path)?;
        trunc.set_len(good_len as u64)?;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    sync_dir(&wal_dir);
    Ok((Wal { file, fsync }, batches))
}

/// Fsyncs the WAL directory itself, so the log file's *directory entry* is
/// durable and not just its contents: fsyncing a freshly created file does
/// not persist the name that points at it, and a crash could otherwise
/// leave a log that no longer exists.
///
/// Best-effort by design. Opening a directory read-only and fsyncing it is
/// the portable-enough POSIX idiom, but some filesystems reject it, and a
/// refusal here must never fail an open that has otherwise succeeded.
fn sync_dir(wal_dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(wal_dir) {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = wal_dir;
}

/// Replays length-prefixed, CRC32-checked frames from `bytes` in order,
/// stopping at the first torn or corrupt frame. Returns the decoded
/// batches and the byte offset one past the last good frame (`bytes.len()`
/// if every frame replayed cleanly).
///
/// Revisions must be strictly increasing from 1: the writer assigns
/// `rev + 1` per committed batch and appends under an exclusive handle, so
/// a repeated or out-of-order rev means the log is not what it claims to be
/// (two writers, or a bug) rather than merely truncated. That is
/// `ClogError::Corrupt`, not a tail to quarantine.
fn replay(bytes: &[u8]) -> Result<(Vec<Batch>, usize), ClogError> {
    let mut batches: Vec<Batch> = Vec::new();
    let mut offset = 0usize;
    let mut last_rev = 0u64;

    while offset < bytes.len() {
        if offset + HEADER_LEN > bytes.len() {
            break; // torn header: not enough bytes for len+crc
        }
        let len_bytes: [u8; 4] = match bytes[offset..offset + 4].try_into() {
            Ok(b) => b,
            Err(_) => break,
        };
        let crc_bytes: [u8; 4] = match bytes[offset + 4..offset + 8].try_into() {
            Ok(b) => b,
            Err(_) => break,
        };
        let len = u32::from_le_bytes(len_bytes) as usize;
        let crc = u32::from_le_bytes(crc_bytes);

        let payload_start = offset + HEADER_LEN;
        let remaining = bytes.len() - payload_start;
        if len > remaining {
            break; // absurd length or torn payload
        }
        let payload = &bytes[payload_start..payload_start + len];
        if crc32fast::hash(payload) != crc {
            break; // corrupt record: never applied
        }
        match postcard::from_bytes::<Batch>(payload) {
            Ok(batch) => {
                if batch.rev <= last_rev {
                    return Err(ClogError::Corrupt {
                        detail: format!(
                            "wal rev not strictly increasing: {} after {last_rev}",
                            batch.rev
                        ),
                    });
                }
                last_rev = batch.rev;
                batches.push(batch);
                offset = payload_start + len;
            }
            // Defensive: a CRC-valid record that still fails to decode is
            // treated the same as a corrupt tail. Should not happen.
            Err(_) => break,
        }
    }

    Ok((batches, offset))
}

/// Appends `tail` to `wal_dir/wal.corrupt`, creating the file if needed.
fn quarantine(wal_dir: &Path, tail: &[u8]) -> Result<(), ClogError> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(wal_dir.join("wal.corrupt"))?;
    f.write_all(tail)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Batch, Event};

    fn batch(rev: u64) -> Batch {
        Batch {
            rev,
            as_of: 1_000 * rev,
            events: vec![Event::Tick { epoch: rev }],
        }
    }

    #[test]
    fn self_review_edge_cases() {
        // empty file: replay finds nothing, no corruption.
        let dir = tempfile::tempdir().unwrap();
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert!(replayed.is_empty());
        assert!(!dir.path().join("wal").join("wal.corrupt").exists());

        // exactly an 8-byte header with no payload following: torn header
        // is impossible here (8 bytes *is* a full header), but a payload
        // of len=0 whose CRC matches empty bytes should still fail to
        // decode as a Batch and be treated as corrupt (defensive path).
        let log = dir.path().join("wal").join("log");
        let crc = crc32fast::hash(&[]);
        let mut frame = Vec::new();
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&crc.to_le_bytes());
        std::fs::write(&log, &frame).unwrap();
        assert_eq!(frame.len(), HEADER_LEN);
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert!(replayed.is_empty());
        assert!(dir.path().join("wal").join("wal.corrupt").exists());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
    }

    #[test]
    fn append_and_replay_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
            assert!(replayed.is_empty());
            w.append(&batch(1)).unwrap();
            w.append(&batch(2)).unwrap();
        }
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert_eq!(
            replayed.iter().map(|b| b.rev).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn r2_torn_tail_truncated_and_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, _) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
            w.append(&batch(1)).unwrap();
            w.append(&batch(2)).unwrap();
        }
        // tear the last record: chop 3 bytes off the file
        let log = dir.path().join("wal").join("log");
        let len = std::fs::metadata(&log).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&log).unwrap();
        f.set_len(len - 3).unwrap();
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert_eq!(replayed.iter().map(|b| b.rev).collect::<Vec<_>>(), vec![1]);
        assert!(dir.path().join("wal").join("wal.corrupt").exists());
        // reopening again is clean (tail already truncated)
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert_eq!(replayed.len(), 1);
    }

    #[test]
    fn non_increasing_rev_is_corrupt_not_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, _) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
            w.append(&batch(1)).unwrap();
            w.append(&batch(1)).unwrap(); // same rev twice: impossible for one writer
        }
        assert!(matches!(
            open_dir(dir.path(), crate::FsyncPolicy::OnCommit),
            Err(ClogError::Corrupt { .. })
        ));

        // rev 0 is likewise impossible: the first committed batch is rev 1.
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, _) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
            w.append(&batch(0)).unwrap();
        }
        assert!(matches!(
            open_dir(dir.path(), crate::FsyncPolicy::OnCommit),
            Err(ClogError::Corrupt { .. })
        ));
    }

    #[test]
    fn r2_corrupt_crc_never_applied_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, _) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
            w.append(&batch(1)).unwrap();
            w.append(&batch(2)).unwrap();
        }
        let log = dir.path().join("wal").join("log");
        // flip a byte in the last record's payload
        let mut bytes = std::fs::read(&log).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF;
        std::fs::write(&log, &bytes).unwrap();
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert_eq!(replayed.iter().map(|b| b.rev).collect::<Vec<_>>(), vec![1]);
    }
}
