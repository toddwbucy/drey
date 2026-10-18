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

/// Returns None when a newer durable snapshot supersedes this entire WAL.
pub(super) fn verify(dir: &Path, wal: &[u8], snapshot_epoch: u64) -> Result<Option<usize>> {
    // Preserve explicit version refusal even when a legacy directory has no seal.
    if wal.len() >= 8 && &wal[..4] == MAGIC {
        let found = u32::from_le_bytes(wal[4..8].try_into().unwrap());
        if found != FORMAT_VERSION {
            return Err(Error::VersionMismatch {
                found,
                supported: FORMAT_VERSION,
            });
        }
    }
    let bytes = fs::read(dir.join(FILE))
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
    let epoch = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if epoch < snapshot_epoch {
        return Ok(None);
    }
    if epoch > snapshot_epoch {
        return Err(Error::GenerationMismatch {
            wal_epoch: epoch,
            snapshot_epoch,
        });
    }
    let len = usize::try_from(u64::from_le_bytes(bytes[16..24].try_into().unwrap()))
        .map_err(|_| Error::Codec("commit seal length overflow".into()))?;
    if len < WAL_HEADER_LEN || len > wal.len() || Sha256::digest(&wal[..len])[..] != bytes[24..56] {
        return Err(Error::Codec(
            "acknowledged WAL bytes missing or corrupt; history left untouched".into(),
        ));
    }
    Ok(Some(len))
}
