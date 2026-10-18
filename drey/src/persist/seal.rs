//! A durable acknowledgement boundary independent of WAL framing.
//! A crash can publish either seal; bytes inside the selected seal must match
//! exactly. Damage cannot be reclassified as an unacknowledged tail.
use super::*;
use sha2::{Digest, Sha256};

pub(super) const FILE: &str = "commit.seal";
pub(super) const TMP: &str = "commit.seal.tmp";

pub(super) fn write(dir: &Path, epoch: u64, len: u64, hash: &Sha256) -> Result<()> {
    let mut bytes = b"DRCS".to_vec();
    bytes.extend(FORMAT_VERSION.to_le_bytes());
    bytes.extend(epoch.to_le_bytes());
    bytes.extend(len.to_le_bytes());
    bytes.extend(hash.clone().finalize());
    bytes.extend(Sha256::digest(&bytes));
    #[cfg(test)]
    fail::hit(&fail::SEAL_WRITE)?;
    write_file_atomic(&dir.join(FILE), &dir.join(TMP), &bytes)?;
    #[cfg(test)]
    fail::hit(&fail::SEAL_PUBLISHED)?;
    fsync_dir(dir)
}

/// A checksum-validated seal. The WAL prefix still needs verification before
/// replay, and the snapshot still needs full validation before stale-WAL repair.
pub(super) struct Seal {
    epoch: u64,
    len: u64,
    hash: [u8; 32],
}

pub(super) fn read(dir: &Path, wal_header: &[u8]) -> Result<Seal> {
    // Preserve explicit version refusal even when a legacy directory has no seal.
    if wal_header.len() >= 8 && &wal_header[..4] == MAGIC {
        let found = u32::from_le_bytes(wal_header[4..8].try_into().unwrap());
        if found != FORMAT_VERSION {
            return Err(Error::VersionMismatch {
                found,
                supported: FORMAT_VERSION,
            });
        }
    }
    // One extra byte detects oversized files without allocating for their body.
    let bytes = (|| -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(89);
        File::open(dir.join(FILE))?
            .take(89)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })()
    .map_err(|e| Error::Storage(format!("commit seal unavailable: {e}")))?;
    if bytes.len() != 88
        || &bytes[..4] != b"DRCS"
        || bytes[4..8] != FORMAT_VERSION.to_le_bytes()
        || Sha256::digest(&bytes[..56])[..] != bytes[56..]
    {
        return Err(Error::Codec(
            "invalid commit seal; history left untouched".into(),
        ));
    }
    Ok(Seal {
        epoch: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        len: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        hash: bytes[24..56].try_into().unwrap(),
    })
}

impl Seal {
    /// Rejection only: an unvalidated snapshot epoch must NEVER authorize
    /// accepting state or discarding WAL bytes. A damaged prefix can also
    /// produce this error; full snapshot validation remains required otherwise.
    pub(super) fn reject_newer_epoch(&self, snapshot_epoch: u64) -> Result<()> {
        if self.epoch > snapshot_epoch {
            return Err(Error::GenerationMismatch {
                wal_epoch: self.epoch,
                snapshot_epoch,
            });
        }
        Ok(())
    }

    /// Returns None only after the caller has fully validated a newer snapshot.
    pub(super) fn verify(&self, wal: &[u8], snapshot_epoch: u64) -> Result<Option<usize>> {
        self.reject_newer_epoch(snapshot_epoch)?;
        if self.epoch < snapshot_epoch {
            return Ok(None);
        }
        let len = usize::try_from(self.len)
            .map_err(|_| Error::Codec("commit seal length overflow".into()))?;
        if len < WAL_HEADER_LEN || len > wal.len() || Sha256::digest(&wal[..len])[..] != self.hash {
            return Err(Error::Codec(
                "acknowledged WAL bytes missing or corrupt; history left untouched".into(),
            ));
        }
        Ok(Some(len))
    }
}
