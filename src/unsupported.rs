//! Fallback for any OS without a backend — always Unsupported{PlatformBuild}.
use std::path::Path;

use crate::{Error, Support, UnsupportedReason};

pub(crate) fn detect(_path: &Path) -> Result<Support, Error> {
  Ok(Support::Unsupported(UnsupportedReason::PlatformBuild))
}

pub(crate) fn is_already_compressed(_path: &Path) -> Result<bool, Error> {
  Ok(false)
}

pub(crate) fn apply_inplace(_path: &Path) -> Result<(), Error> {
  Ok(())
}

/// No FS-specific on-disk signal — apply_guarded falls back to the generic
/// allocated-bytes measurement (st_blocks / GetCompressedFileSizeW), which DOES
/// reflect the win on APFS and NTFS.
pub(crate) fn compressed_on_disk(_path: &Path) -> Result<Option<bool>, Error> {
  Ok(None)
}
