//! `decmpfs` — the napi-free core behind the self-loading compressed-addon stub.
//!
//! A `--compress` addon ships as ONE `.node` = `[stub image][zstd payload][footer]`.
//! Node `dlopen`s the stub; `resolve_self` decodes the appended payload and, on a
//! compressing FS (macOS APFS / Linux btrfs / Windows NTFS), rewrites the file in
//! place into the raw FS-compressed addon — so every later load is Node reading the
//! real addon directly at near-native speed (kernel decompress-on-read). Where the FS
//! can't compress, it decodes to the ephemeral cache (`NAPI_RS_NATIVE_CACHE`).
//! Self-replacement uses the updater rename-dance, uniform across platforms (see
//! `replace_self`).
//!
//! Backends: btrfs (`FS_COMPR_FL`), NTFS (`FSCTL_SET_COMPRESSION`), and macOS decmpfs
//! (resource fork, kernel-roundtrip verified); other targets report `Unsupported`
//! and take the cache path.
//!
//! Contract: every `Outcome` is a SUCCESS; `Err` is reserved for genuine I/O failures
//! that leave the file's integrity unknown. An unsupported FS, a permission/lock
//! issue, an incompressible or too-large file are non-fatal `Outcome`s.
//!
//! Panic-free invariant: the stub ships built with `panic=abort` (the `addon-min`
//! profile), so a panic can't unwind — it would kill the host process. The deny below
//! keeps non-test code free of the obvious panic sources; all slice indexing is
//! length-guarded.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::path::Path;

/// What happened to the file. Only `Err` is a hard failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
  /// Applied and on-disk allocation actually decreased.
  Compressed { before: u64, after: u64 },
  /// Applied (or already set) but on-disk size did not drop — incompressible
  /// or sub-cluster. Content is byte-identical and fully loadable.
  NoGain { before: u64, after: u64 },
  /// Already carried the compression flag/xattr before we touched it.
  AlreadyCompressed { before: u64 },
  /// This FS/OS has no per-file transparent compression (ext4, xfs, ZFS, ReFS,
  /// FAT, tmpfs, overlay/network mounts). Caller falls through to the cache.
  Unsupported { reason: UnsupportedReason },
  /// Detected support but could not apply (permissions, lock, immutable,
  /// rollback). Warn-and-continue; never a hard error.
  Skipped { reason: SkipReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedReason {
  /// Filesystem (by allowlist) has no transparent compression.
  Filesystem,
  /// Network/overlay/bind mount where the signal is unreliable.
  NetworkOrOverlay,
  /// Built for an OS with no backend (or skeleton: not yet implemented).
  PlatformBuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
  /// EACCES / EPERM / EROFS — read-only or unowned (e.g. unprivileged container).
  PermissionDenied,
  /// A write handle is held / ETXTBSY / sharing violation; could not lock.
  Busy,
  /// UF_IMMUTABLE / SF_IMMUTABLE and we declined to toggle it.
  Immutable,
  /// EFS / FILE_ATTRIBUTE_ENCRYPTED.
  Encrypted,
  /// Applied, structural verification failed, rolled back to the original.
  IntegrityRevert,
  /// Post-apply loadability (magic-bytes) check failed, rolled back.
  NotLoadable,
  /// Exceeds a backend limit (e.g. decmpfs u32 offsets cap at 4 GiB).
  TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
  Supported,
  AlreadyCompressed,
  Unsupported(UnsupportedReason),
}

/// Genuine failures only. A capability/permission gap is an `Outcome`, not an `Error`.
#[derive(Debug)]
pub enum Error {
  Io {
    context: &'static str,
    source: std::io::Error,
  },
  NotFound(std::path::PathBuf),
}

impl std::fmt::Display for Error {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Error::Io { context, source } => write!(f, "io error at {context}: {source}"),
      Error::NotFound(p) => write!(f, "file not found: {}", p.display()),
    }
  }
}

impl std::error::Error for Error {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Error::Io { source, .. } => Some(source),
      Error::NotFound(_) => None,
    }
  }
}

/// First bytes of a raw, loadable native module: ELF, Mach-O (32/64, LE/BE,
/// universal), or PE. Used to verify a decoded payload will actually load.
fn is_native_binary(head: &[u8]) -> bool {
  if head.len() < 4 {
    return false;
  }
  matches!(
    [head[0], head[1], head[2], head[3]],
    [0x7f, 0x45, 0x4c, 0x46]        // ELF
            | [0xcf, 0xfa, 0xed, 0xfe]  // Mach-O 64 LE
            | [0xfe, 0xed, 0xfa, 0xcf]  // Mach-O 64 BE
            | [0xce, 0xfa, 0xed, 0xfe]  // Mach-O 32 LE
            | [0xfe, 0xed, 0xfa, 0xce]  // Mach-O 32 BE
            | [0xca, 0xfe, 0xba, 0xbe]  // Mach-O universal
            | [0xbe, 0xba, 0xfe, 0xca] // Mach-O universal (byte-swapped)
  ) || (head[0] == 0x4d && head[1] == 0x5a) // PE ("MZ")
}

/// Trailer the producer appends: `[payload_len u64 LE][content_hash u64 LE][magic]`.
/// `content_hash` is FNV-1a of the RAW addon (what the cache file holds), stamped at
/// build time. It lets the warm cache path name + find the addon WITHOUT reading or
/// decoding the payload — only these 24 bytes are read on a hit.
const STUB_FOOTER_MAGIC: &[u8; 8] = b"NAPCSTUB";
const STUB_FOOTER_SIZE: u64 = 24;

struct Footer {
  payload_len: u64,
  content_hash: u64,
}

/// Wrap the last OS error with context — shared by every backend.
pub(crate) fn io(context: &'static str) -> Error {
  Error::Io {
    context,
    source: std::io::Error::last_os_error(),
  }
}

/// A NUL-checked C string from a path, for the unix backends that hand paths to
/// libc.
#[cfg(unix)]
pub(crate) fn cstring(path: &Path) -> Result<std::ffi::CString, Error> {
  use std::os::unix::ffi::OsStrExt;
  std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Io {
    context: "path has interior NUL",
    source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
  })
}

/// THE stub entry point — performance-tuned. Reads its own 24-byte trailer to locate
/// the appended zstd payload + its content hash, then takes the cheapest path:
///   - non-compressing FS with a warm cache → return the cached path having read only
///     the footer + done one `stat` — NO payload read, NO decode, NO dir sweep;
///   - otherwise read + decode the payload, and on a compressing FS rewrite the stub
///     in place into the raw FS-compressed addon (so every later load is Node reading
///     the real addon directly), else write the ephemeral cache.
///
/// Sweep + the expensive work happen only on a real miss, never on the hot path.
pub fn resolve_self(self_path: &Path) -> Option<std::path::PathBuf> {
  use std::io::{Read, Seek, SeekFrom};

  let mut file = std::fs::File::open(self_path).ok()?;
  let total = file.seek(SeekFrom::End(0)).ok()?;
  let footer = read_footer(&mut file, total)?;

  // FAST PATH: a warm cache hit needs nothing but the footer + one stat — no probe
  // (statfs), no payload read, no decode. The cache file is named by the footer's
  // content hash, so it resolves without touching the FS backend. On a compressing
  // FS the file self-rewrites on first load and the stub never runs again, so no
  // cache is created there — this only fires on the cache path.
  if let Some(hit) = cache_path(self_path, footer.content_hash).filter(|p| p.exists()) {
    return Some(hit);
  }

  // Cold miss: probe to route — rewrite in place on a compressing FS (once), else
  // decode to the cache. Read + decode the payload.
  let compressing = matches!(
    probe(self_path),
    Ok(Support::Supported) | Ok(Support::AlreadyCompressed)
  );
  let start = total
    .checked_sub(STUB_FOOTER_SIZE)?
    .checked_sub(footer.payload_len)?;
  file.seek(SeekFrom::Start(start)).ok()?;
  let mut payload = vec![0u8; usize::try_from(footer.payload_len).ok()?];
  file.read_exact(&mut payload).ok()?;
  let raw = decode(&payload)?;
  if !is_native_binary(&raw) {
    return None;
  }

  // Real work → sweep leftover .old here, off the cache-hit fast path.
  if let Some(dir) = self_path.parent() {
    sweep_old(dir);
  }
  // On a compressing FS, rewrite self_path into the raw FS-compressed addon so
  // EVERY FUTURE process `dlopen`s it directly — no stub, no cache. But THIS
  // process is mid-`dlopen` of self_path (that's how the stub got here), so it
  // cannot load self_path now (`dlopen` returns this in-flight stub). So it always
  // loads the addon from the cache, which the next line writes. Future loads never
  // touch the cache; it's the rewriting process's one-time expansion.
  if compressing {
    if let Some(()) = replace_self(self_path, &raw) {
      let _ = compress_file(self_path);
    }
  }
  write_cache(self_path, footer.content_hash, &raw)
}

/// Read + validate the 24-byte trailer.
fn read_footer(file: &mut std::fs::File, total: u64) -> Option<Footer> {
  use std::io::{Read, Seek, SeekFrom};
  if total < STUB_FOOTER_SIZE {
    return None;
  }
  file.seek(SeekFrom::End(-(STUB_FOOTER_SIZE as i64))).ok()?;
  let mut buf = [0u8; STUB_FOOTER_SIZE as usize];
  file.read_exact(&mut buf).ok()?;
  if &buf[16..24] != STUB_FOOTER_MAGIC {
    return None;
  }
  Some(Footer {
    payload_len: u64::from_le_bytes(buf[0..8].try_into().ok()?),
    content_hash: u64::from_le_bytes(buf[8..16].try_into().ok()?),
  })
}

/// The content-addressed cache path for `hash` — computable from the footer alone, so
/// the warm path never decodes. Location follows `NAPI_RS_NATIVE_CACHE`; `=0` uses a
/// per-process name so disk never holds more than the blob plus one expansion.
fn cache_path(self_path: &Path, hash: u64) -> Option<std::path::PathBuf> {
  let dir = resolve_cache_dir(self_path)?;
  let stem = self_path.file_stem()?.to_string_lossy();
  let no_cache = matches!(
    std::env::var("NAPI_RS_NATIVE_CACHE").as_deref(),
    Ok("0") | Ok("false") | Ok("none")
  );
  Some(if no_cache {
    dir.join(format!("{stem}-{}.node", std::process::id()))
  } else {
    dir.join(format!("{stem}-{hash:016x}.node"))
  })
}

/// Decode-and-write a cache miss: write the raw addon to its cache path, then
/// FS-compress it where supported (so loading the cache file is also OS
/// decompress-on-read).
fn write_cache(self_path: &Path, hash: u64, raw: &[u8]) -> Option<std::path::PathBuf> {
  let cache_file = cache_path(self_path, hash)?;
  std::fs::create_dir_all(cache_file.parent()?).ok()?;
  write_atomic(&cache_file, raw)?;
  let _ = compress_file(&cache_file);
  Some(cache_file)
}

/// Best-effort sweep of leftover `*.node.<pid>.old` stubs — from a crashed run, or a
/// Windows delete-on-close that hasn't fired yet. Still-loaded ones just fail to
/// delete; ignored. Only ever called when doing real work, never on the fast path.
fn sweep_old(dir: &Path) {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let name = entry.file_name();
    let name = name.to_string_lossy();
    if name.ends_with(".old") && name.contains(".node.") {
      let _ = std::fs::remove_file(entry.path());
    }
  }
}

/// Cheap content-address (FNV-1a 64) the producer runs over the RAW addon and stamps
/// into the footer — a cache-lookup key, not a security boundary (npm + the FS cover
/// integrity), so no crypto dep; collisions across a node_modules are irrelevant.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
  let mut hash = 0xcbf2_9ce4_8422_2325u64;
  for &b in bytes {
    hash ^= b as u64;
    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
  }
  hash
}

/// Replace the loaded stub file with `raw`.
///
/// WHY the rename dance (and not the alternatives):
///   - rename-OVER self (`write temp; rename(temp, self)`) works on unix — the
///     loaded inode survives — but FAILS on Windows: you can't delete/replace a
///     loaded DLL. So it can't be the one cross-platform path.
///   - decode to a separate cache file and load THAT keeps the stub composite and
///     leaves TWO files on disk (composite + cache). We want one file.
///   - rename the loaded file AWAY, then write the new one in its place, is allowed
///     for a loaded module on BOTH unix and Windows (the open handle tracks the file
///     object, not the name). It's exactly what self-updating installers do, ends at
///     a single file, and gives one code path for every platform — so that's what we
///     use. Cleanup of the renamed-away file is the only per-OS bit (see cleanup_old).
fn replace_self(self_path: &Path, raw: &[u8]) -> Option<()> {
  let old = self_path.with_file_name(format!(
    "{}.{}.old",
    self_path.file_name()?.to_string_lossy(),
    std::process::id()
  ));
  std::fs::rename(self_path, &old).ok()?;
  if write_atomic(self_path, raw).is_none() {
    let _ = std::fs::rename(&old, self_path); // roll back
    return None;
  }
  cleanup_old(&old);
  Some(())
}

/// Remove the renamed-away old stub. Unix unlinks a still-mapped file immediately
/// (the inode survives until unmapped); Windows can't delete a loaded DLL, so mark it
/// delete-on-close — it vanishes when the process exits (no reboot, no admin).
fn cleanup_old(old: &Path) {
  if std::fs::remove_file(old).is_ok() {}
  #[cfg(windows)]
  windows_delete_on_close(old);
}

/// Mark `path` (the renamed-away, still-loaded stub) for deletion when its last
/// handle closes — at process exit. Uses POSIX-semantics disposition so the name is
/// freed immediately and no reboot/admin is needed.
#[cfg(windows)]
fn windows_delete_on_close(path: &Path) {
  use std::os::windows::ffi::OsStrExt;

  use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
  use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileDispositionInfoEx, SetFileInformationByHandle, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
  };

  let wide: Vec<u16> = path
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .collect();
  let handle: HANDLE = unsafe {
    CreateFileW(
      wide.as_ptr(),
      GENERIC_WRITE,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
      std::ptr::null(),
      OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS,
      std::ptr::null_mut(),
    )
  };
  if handle == INVALID_HANDLE_VALUE || handle.is_null() {
    return;
  }
  let mut info = FILE_DISPOSITION_INFO_EX {
    Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
  };
  unsafe {
    SetFileInformationByHandle(
      handle,
      FileDispositionInfoEx,
      (&mut info as *mut FILE_DISPOSITION_INFO_EX).cast(),
      std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
    );
    CloseHandle(handle);
  }
}

/// Per-uid cache subdir. The warm path trusts its own cache file (it `dlopen`s it
/// without re-decoding) — the exact trust model as Node's own compile cache, which
/// writes to the same OS temp dir. So we mirror Node's mitigation: namespace the
/// directory by uid so users don't share one in a world-writable `/tmp` (the
/// cache-poisoning surface). Node v26.3.1 `src/compile_cache.cc` `GetCacheKey`
/// appends `getuid()` for the same reason; `%TEMP%` is already per-user on Windows.
/// Ref: https://github.com/nodejs/node/blob/v26.3.1/src/compile_cache.cc
fn cache_subdir() -> String {
  #[cfg(any(target_os = "linux", target_os = "macos"))]
  {
    format!("napi-rs-native-{}", unsafe { libc::getuid() })
  }
  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  {
    String::from("napi-rs-native")
  }
}

/// Resolve the fallback cache directory from `NAPI_RS_NATIVE_CACHE`:
/// unset / `tmpdir` → OS temp; `node_modules` → nearest `node_modules/.cache`;
/// `workspace` → workspace-root `node_modules/.cache`; `0`/`false`/`none` → OS temp
/// (per-process file); any other value → that path. Only the FALLBACK uses this;
/// the OS path overwrites the payload in place and never touches the cache.
fn resolve_cache_dir(payload: &Path) -> Option<std::path::PathBuf> {
  let subdir = cache_subdir();
  match std::env::var("NAPI_RS_NATIVE_CACHE").as_deref() {
    Ok("0") | Ok("false") | Ok("none") => Some(std::env::temp_dir()),
    Ok("node_modules") => Some(nearest_node_modules(payload)?.join(".cache").join(&subdir)),
    Ok("workspace") => Some(
      workspace_node_modules(payload)?
        .join(".cache")
        .join(&subdir),
    ),
    Ok(path) if !path.is_empty() && path != "tmpdir" => Some(Path::new(path).join(&subdir)),
    _ => Some(std::env::temp_dir().join(&subdir)),
  }
}

/// Nearest ancestor directory named `node_modules`.
fn nearest_node_modules(start: &Path) -> Option<std::path::PathBuf> {
  let mut dir = start.parent();
  while let Some(d) = dir {
    if d.file_name().is_some_and(|n| n == "node_modules") {
      return Some(d.to_path_buf());
    }
    dir = d.parent();
  }
  None
}

/// The workspace root's `node_modules`, found by a workspace manifest; falls back to
/// the topmost `node_modules` ancestor.
fn workspace_node_modules(start: &Path) -> Option<std::path::PathBuf> {
  const MARKERS: [&str; 5] = [
    "pnpm-workspace.yaml",
    "aube-workspace.yaml",
    "vlt-workspaces.json",
    "lerna.json",
    "rush.json",
  ];
  let mut topmost = None;
  let mut dir = start.parent();
  while let Some(d) = dir {
    if d.file_name().is_some_and(|n| n == "node_modules") {
      topmost = Some(d.to_path_buf());
    }
    if MARKERS.iter().any(|m| d.join(m).exists()) {
      return Some(d.join("node_modules"));
    }
    if std::fs::read_to_string(d.join("package.json")).is_ok_and(|s| s.contains("\"workspaces\"")) {
      return Some(d.join("node_modules"));
    }
    dir = d.parent();
  }
  topmost
}

fn decode(payload: &[u8]) -> Option<Vec<u8>> {
  // libzstd (via the `zstd` crate) — ~25-30x faster than a pure-Rust decoder on
  // the one-time cold first-load; warm loads never decode at all.
  zstd::decode_all(payload).ok()
}

/// Write `data` to a sibling temp file, then rename over `path`. The rename is
/// atomic within the directory and gives a fresh inode — the copy-break that
/// isolates this file from any pnpm CAS hardlink siblings.
fn write_atomic(path: &Path, data: &[u8]) -> Option<()> {
  use std::io::Write;
  let dir = path.parent()?;
  let name = path.file_name()?.to_string_lossy();
  let tmp = dir.join(format!(".{name}.decmpfs-{}.tmp", std::process::id()));
  let write = (|| {
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(data)?;
    file.sync_all()
  })();
  if write.is_err() {
    let _ = std::fs::remove_file(&tmp);
    return None;
  }
  if std::fs::rename(&tmp, path).is_err() {
    let _ = std::fs::remove_file(&tmp);
    return None;
  }
  Some(())
}

/// Detect-only, no mutation — for dry-run / capability reporting.
pub fn probe(path: &Path) -> Result<Support, Error> {
  backend::detect(path)
}

/// THE entry point: detect → gate → apply → verify → rollback-on-failure.
/// Idempotent. Never panics. Never corrupts the file.
pub fn compress_file(path: &Path) -> Result<Outcome, Error> {
  if !path.exists() {
    return Err(Error::NotFound(path.to_path_buf()));
  }
  match backend::detect(path)? {
    Support::Unsupported(reason) => Ok(Outcome::Unsupported { reason }),
    Support::AlreadyCompressed => Ok(Outcome::AlreadyCompressed {
      before: verify::on_disk_bytes(path)?,
    }),
    Support::Supported => safety::apply_guarded(path),
  }
}

mod safety;
mod verify;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod backend;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[path = "unsupported.rs"]
mod backend;

#[cfg(test)]
mod tests {
  use super::*;

  fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("decmpfs-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
  }

  // A minimal "loadable" payload: native magic so is_native_binary accepts it.
  fn fake_addon() -> Vec<u8> {
    let mut raw = vec![0x7f, 0x45, 0x4c, 0x46]; // ELF
    raw.extend_from_slice(&[7u8; 9000]);
    raw
  }

  #[test]
  fn detects_native_magics() {
    assert!(is_native_binary(&[0x7f, 0x45, 0x4c, 0x46, 0, 0])); // ELF
    assert!(is_native_binary(&[0xcf, 0xfa, 0xed, 0xfe])); // Mach-O 64 LE
    assert!(is_native_binary(&[0xfe, 0xed, 0xfa, 0xcf])); // Mach-O 64 BE
    assert!(is_native_binary(&[0xce, 0xfa, 0xed, 0xfe])); // Mach-O 32 LE
    assert!(is_native_binary(&[0xfe, 0xed, 0xfa, 0xce])); // Mach-O 32 BE
    assert!(is_native_binary(&[0xca, 0xfe, 0xba, 0xbe])); // Mach-O universal
    assert!(is_native_binary(&[0xbe, 0xba, 0xfe, 0xca])); // Mach-O universal swapped
    assert!(is_native_binary(&[0x4d, 0x5a, 0x90, 0x00])); // PE "MZ"
    assert!(!is_native_binary(b"NAPC"));
    assert!(!is_native_binary(&[0x00, 0x01]));
    assert!(!is_native_binary(&[0x7f, 0x45])); // too short
  }

  #[test]
  fn read_footer_rejects_a_file_shorter_than_the_trailer() {
    let dir = scratch("short-footer");
    let path = dir.join("tiny.node");
    std::fs::write(&path, [0u8; 8]).unwrap(); // < 24-byte footer
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(&path).unwrap();
    let total = file.seek(SeekFrom::End(0)).unwrap();
    assert!(read_footer(&mut file, total).is_none());
    assert!(resolve_self(&path).is_none(), "no footer → None");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn compress_file_errors_when_missing() {
    let p = std::path::Path::new("/no/such/addon.node");
    assert!(matches!(compress_file(p), Err(Error::NotFound(_))));
  }

  #[cfg(unix)]
  #[test]
  fn compress_file_reports_unsupported_on_a_non_compressing_fs() {
    // /dev/null exists but devfs/devtmpfs has no compression backend → Unsupported,
    // exercising compress_file's Unsupported match arm.
    let out = compress_file(std::path::Path::new("/dev/null"));
    assert!(
      matches!(out, Ok(Outcome::Unsupported { .. })),
      "devfs → Unsupported, got {out:?}"
    );
  }

  #[test]
  fn decode_roundtrips_zstd() {
    let raw = fake_addon();
    let payload = zstd::encode_all(&raw[..], 3).unwrap();
    assert_eq!(decode(&payload).unwrap(), raw);
  }

  // NAPI_RS_NATIVE_CACHE is process-global; serialize the tests that touch it.
  static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

  // A self-loading stub file: [fake stub image][zstd payload][footer], where
  // footer = [payload_len u64][content_hash u64][magic] and content_hash is over raw.
  fn stub_composite(raw: &[u8]) -> Vec<u8> {
    let payload = zstd::encode_all(raw, 3).unwrap();
    let mut file = vec![0x7f, 0x45, 0x4c, 0x46]; // fake stub image (ELF magic)
    file.extend_from_slice(&[0u8; 200]);
    file.extend_from_slice(&payload);
    file.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    file.extend_from_slice(&fnv1a64(raw).to_le_bytes());
    file.extend_from_slice(STUB_FOOTER_MAGIC);
    file
  }

  #[test]
  fn read_footer_reads_len_and_hash() {
    let raw = fake_addon();
    let payload = zstd::encode_all(&raw[..], 3).unwrap();
    let dir = scratch("read-footer");
    let path = dir.join("addon.node");
    std::fs::write(&path, stub_composite(&raw)).unwrap();

    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(&path).unwrap();
    let total = file.seek(SeekFrom::End(0)).unwrap();
    let footer = read_footer(&mut file, total).expect("footer");
    assert_eq!(footer.payload_len, payload.len() as u64);
    assert_eq!(footer.content_hash, fnv1a64(&raw));

    // A file with no trailer magic → None.
    std::fs::write(&path, fake_addon()).unwrap();
    let mut bad = std::fs::File::open(&path).unwrap();
    let total = bad.seek(SeekFrom::End(0)).unwrap();
    assert!(read_footer(&mut bad, total).is_none());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolve_self_self_rewrites_on_a_compressing_fs() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = scratch("resolve-rewrite");
    let cache = dir.join("cache");
    std::env::set_var("NAPI_RS_NATIVE_CACHE", &cache);
    let path = dir.join("addon.node");
    let raw = fake_addon();
    std::fs::write(&path, stub_composite(&raw)).unwrap();

    // The host temp is APFS (Supported); btrfs/NTFS take the same branch in CI. On
    // a non-compressing FS this skips (the cache branch is covered separately).
    if !matches!(probe(&path), Ok(Support::Supported)) {
      std::env::remove_var("NAPI_RS_NATIVE_CACHE");
      std::fs::remove_dir_all(&dir).ok();
      return;
    }
    let loaded = resolve_self(&path).expect("resolve_self self-rewrite");
    // self_path is rewritten into the raw FS-compressed addon — future processes
    // dlopen it directly (no stub). Kernel decompress-on-read returns the original.
    assert_eq!(
      std::fs::read(&path).unwrap(),
      raw,
      "self_path is now the raw addon"
    );
    assert!(
      matches!(
        compress_file(&path).unwrap(),
        Outcome::AlreadyCompressed { .. }
      ),
      "the rewritten file is FS-compressed"
    );
    // THIS process can't reload self_path (it's mid-dlopen), so resolve_self hands
    // back the cache, which is also loadable.
    assert!(
      loaded.starts_with(&cache),
      "current load via cache: {loaded:?}"
    );
    assert_eq!(
      std::fs::read(&loaded).unwrap(),
      raw,
      "cache holds the raw addon"
    );

    std::env::remove_var("NAPI_RS_NATIVE_CACHE");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn resolve_self_falls_back_to_cache_when_the_self_rewrite_is_blocked() {
    // If the addon can't overwrite itself (no write permission on its directory),
    // the in-place self-rewrite is skipped and the load STILL succeeds via the
    // ephemeral cache — graceful fail, never None. Root bypasses dir perms, so skip.
    let _guard = ENV_LOCK.lock().unwrap();
    if unsafe { libc::geteuid() } == 0 {
      return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("resolve-ro-rewrite");
    let cache = dir.join("cache");
    std::env::set_var("NAPI_RS_NATIVE_CACHE", &cache);
    // The addon lives in its own subdir we can make read-only without blocking the
    // cache (which is elsewhere under `dir`).
    let nm = dir.join("nm");
    std::fs::create_dir_all(&nm).unwrap();
    let path = nm.join("addon.node");
    let raw = fake_addon();
    std::fs::write(&path, stub_composite(&raw)).unwrap();

    // Read-only dir → replace_self's rename can't happen, so the rewrite is blocked.
    std::fs::set_permissions(&nm, std::fs::Permissions::from_mode(0o555)).unwrap();
    let loaded = resolve_self(&path);
    std::fs::set_permissions(&nm, std::fs::Permissions::from_mode(0o755)).ok();

    let loaded = loaded.expect("blocked self-rewrite falls back to the cache, not None");
    assert!(
      loaded.starts_with(&cache),
      "loaded via the cache: {loaded:?}"
    );
    assert_eq!(
      std::fs::read(&loaded).unwrap(),
      raw,
      "the cache holds the decoded addon"
    );
    assert_eq!(
      std::fs::read(&path).unwrap(),
      stub_composite(&raw),
      "self_path is left untouched when the rewrite is blocked"
    );

    std::env::remove_var("NAPI_RS_NATIVE_CACHE");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolve_self_returns_a_warm_cache_hit() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = scratch("resolve-hit");
    let cache = dir.join("cache");
    std::env::set_var("NAPI_RS_NATIVE_CACHE", &cache);
    let path = dir.join("addon.node");
    let raw = fake_addon();
    std::fs::write(&path, stub_composite(&raw)).unwrap();

    // Pre-seed the content-addressed cache file. resolve_self checks the cache
    // (named by the footer hash) BEFORE probing the FS, so it returns the hit
    // without reading/decoding the payload or self-rewriting — even on a
    // compressing FS.
    let hit = cache_path(&path, fnv1a64(&raw)).unwrap();
    std::fs::create_dir_all(hit.parent().unwrap()).unwrap();
    std::fs::write(&hit, &raw).unwrap();

    assert_eq!(resolve_self(&path).as_ref(), Some(&hit), "warm cache hit");
    assert_ne!(
      resolve_self(&path).unwrap(),
      path,
      "no self-rewrite on a hit"
    );

    std::env::remove_var("NAPI_RS_NATIVE_CACHE");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn cache_dir_defaults_to_tmp_and_honors_overrides() {
    let _guard = ENV_LOCK.lock().unwrap();
    let p = std::path::Path::new("/whatever/addon.node");

    std::env::remove_var("NAPI_RS_NATIVE_CACHE");
    assert_eq!(
      resolve_cache_dir(p),
      Some(std::env::temp_dir().join(cache_subdir()))
    );
    std::env::set_var("NAPI_RS_NATIVE_CACHE", "tmpdir");
    assert_eq!(
      resolve_cache_dir(p),
      Some(std::env::temp_dir().join(cache_subdir()))
    );
    std::env::set_var("NAPI_RS_NATIVE_CACHE", "/custom/dir");
    assert_eq!(
      resolve_cache_dir(p),
      Some(std::path::PathBuf::from("/custom/dir").join(cache_subdir()))
    );
    // The /tmp subdir is per-uid, so users never share a cache there.
    assert!(cache_subdir().contains("napi-rs-native"));
    // 0 / false / none → the bare temp dir (no persistent subdir).
    for off in ["0", "false", "none"] {
      std::env::set_var("NAPI_RS_NATIVE_CACHE", off);
      assert_eq!(resolve_cache_dir(p), Some(std::env::temp_dir()), "{off}");
    }
    std::env::remove_var("NAPI_RS_NATIVE_CACHE");
  }

  #[test]
  fn cache_dir_resolves_node_modules_and_workspace_modes() {
    let _guard = ENV_LOCK.lock().unwrap();
    let root = scratch("cache-modes");
    let nm = root.join("node_modules");
    let pkg = nm.join("addon");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(root.join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
    let file = pkg.join("addon.node");
    let sub = std::path::Path::new(".cache").join(cache_subdir());

    std::env::set_var("NAPI_RS_NATIVE_CACHE", "node_modules");
    assert_eq!(resolve_cache_dir(&file), Some(nm.join(&sub)));
    std::env::set_var("NAPI_RS_NATIVE_CACHE", "workspace");
    assert_eq!(resolve_cache_dir(&file), Some(nm.join(&sub)));
    std::env::remove_var("NAPI_RS_NATIVE_CACHE");
    std::fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn cache_path_uses_a_per_process_name_when_caching_is_off() {
    let _guard = ENV_LOCK.lock().unwrap();
    let p = std::path::Path::new("/tmp/addon.node");
    std::env::set_var("NAPI_RS_NATIVE_CACHE", "0");
    let path = cache_path(p, 0xdead_beef).unwrap();
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(
      name.contains(&std::process::id().to_string()),
      "no-cache uses a per-pid name: {name}"
    );
    std::env::set_var("NAPI_RS_NATIVE_CACHE", "tmpdir");
    let hashed = cache_path(p, 0xdead_beef).unwrap();
    assert!(
      hashed
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains("deadbeef"),
      "default names by content hash"
    );
    std::env::remove_var("NAPI_RS_NATIVE_CACHE");
  }

  #[test]
  fn nearest_and_workspace_node_modules() {
    let root = scratch("ws");
    let nm = root.join("node_modules");
    let pkg = nm
      .join(".pnpm")
      .join("dep@1")
      .join("node_modules")
      .join("addon");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(root.join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
    let file = pkg.join("addon.node");
    let inner_nm = pkg.parent().unwrap().to_path_buf(); // .../.pnpm/dep@1/node_modules

    // nearest: the innermost node_modules ancestor.
    assert_eq!(nearest_node_modules(&file), Some(inner_nm));
    // workspace: the manifest root's node_modules.
    assert_eq!(workspace_node_modules(&file), Some(nm));
    std::fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn decode_is_none_on_corrupt_payload() {
    assert!(decode(b"not a zstd frame").is_none());
  }

  #[test]
  fn resolve_self_keeps_the_original_when_decode_is_garbage() {
    let dir = scratch("resolve-bad");
    let path = dir.join("addon.node");
    // A valid footer pointing at junk that isn't a zstd frame.
    let mut file = vec![0x7f, 0x45, 0x4c, 0x46]; // stub image
    file.extend_from_slice(&[0u8; 50]);
    let junk = b"definitely not a zstd frame";
    file.extend_from_slice(junk);
    file.extend_from_slice(&(junk.len() as u64).to_le_bytes());
    file.extend_from_slice(&fnv1a64(junk).to_le_bytes());
    file.extend_from_slice(STUB_FOOTER_MAGIC);
    std::fs::write(&path, &file).unwrap();

    assert!(resolve_self(&path).is_none());
    assert_eq!(std::fs::read(&path).unwrap(), file, "original untouched");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolve_self_rejects_a_non_native_payload() {
    let dir = scratch("not-native");
    let path = dir.join("addon.node");
    // Decodes cleanly, but the bytes aren't a native binary → None.
    let junk = b"this is plain text, not ELF/Mach-O/PE";
    let payload = zstd::encode_all(&junk[..], 3).unwrap();
    let mut file = vec![0x7f, 0x45, 0x4c, 0x46];
    file.extend_from_slice(&[0u8; 50]);
    file.extend_from_slice(&payload);
    file.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    file.extend_from_slice(&fnv1a64(junk).to_le_bytes());
    file.extend_from_slice(STUB_FOOTER_MAGIC);
    std::fs::write(&path, &file).unwrap();
    assert!(resolve_self(&path).is_none());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn write_atomic_fails_when_the_temp_cant_be_created() {
    // A missing parent dir can't hold the sibling temp — fails for ANY user
    // (unlike a read-only dir, which root bypasses).
    let result = write_atomic(std::path::Path::new("/no/such/dir/x.node"), b"data");
    assert!(
      result.is_none(),
      "write_atomic fails when it can't create the temp"
    );
  }

  #[test]
  fn write_atomic_fails_when_the_target_is_a_directory() {
    // The sibling temp is created fine, but rename(temp, target) fails because the
    // target is an existing directory (EISDIR) — covers the rename-failure branch.
    let dir = scratch("wa-rename");
    let target = dir.join("blocked");
    std::fs::create_dir(&target).unwrap();
    assert!(
      write_atomic(&target, b"data").is_none(),
      "write_atomic fails when the rename target is a directory"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn sweep_old_removes_stale_old_stubs_only() {
    let dir = scratch("sweep");
    let old = dir.join("addon.node.4321.old");
    let keep = dir.join("addon.node");
    let other = dir.join("notes.txt");
    std::fs::write(&old, b"x").unwrap();
    std::fs::write(&keep, b"y").unwrap();
    std::fs::write(&other, b"z").unwrap();
    sweep_old(&dir);
    assert!(!old.exists(), "stale .old removed");
    assert!(keep.exists() && other.exists(), "real files kept");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn node_modules_helpers_handle_absence() {
    let _guard = ENV_LOCK.lock().unwrap();
    // No node_modules ancestor at all.
    assert_eq!(
      nearest_node_modules(std::path::Path::new("/tmp/no/nm/here.node")),
      None
    );
    // node_modules present but no workspace marker → topmost node_modules.
    let root = scratch("ws-none");
    let nm = root.join("node_modules").join("pkg");
    std::fs::create_dir_all(&nm).unwrap();
    assert_eq!(
      workspace_node_modules(&nm.join("a.node")),
      Some(root.join("node_modules"))
    );
    std::fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn workspace_node_modules_finds_a_marker_file() {
    let root = scratch("ws-marker");
    let nm = root.join("node_modules").join("pkg");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(root.join("lerna.json"), "{}\n").unwrap();
    assert_eq!(
      workspace_node_modules(&nm.join("a.node")),
      Some(root.join("node_modules"))
    );
    std::fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn workspace_node_modules_finds_package_json_workspaces() {
    let root = scratch("ws-pkg");
    let nm = root.join("node_modules").join("pkg");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(root.join("package.json"), "{\"workspaces\":[\"a\"]}\n").unwrap();
    assert_eq!(
      workspace_node_modules(&nm.join("a.node")),
      Some(root.join("node_modules"))
    );
    std::fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn error_formats_and_chains_its_source() {
    let io = Error::Io {
      context: "doing-x",
      source: std::io::Error::from(std::io::ErrorKind::Other),
    };
    assert!(format!("{io}").contains("doing-x"));
    assert!(std::error::Error::source(&io).is_some());
    let nf = Error::NotFound(std::path::PathBuf::from("/p"));
    assert!(format!("{nf}").contains("not found"));
    assert!(std::error::Error::source(&nf).is_none());
  }

  #[cfg(unix)]
  #[test]
  fn compress_file_skips_a_read_only_file() {
    // On a compressing FS, a read-only file can't be opened rw → the orchestrator
    // fail-soft turns the EACCES into Skipped(PermissionDenied), not a hard error.
    // Root bypasses the mode bits, so skip there (CI containers run as root).
    if unsafe { libc::geteuid() } == 0 {
      return;
    }
    let dir = scratch("ro");
    let path = dir.join("addon.node");
    std::fs::write(&path, fake_addon()).unwrap();
    if !matches!(probe(&path), Ok(Support::Supported)) {
      std::fs::remove_dir_all(&dir).ok();
      return;
    }
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(&path, perm).unwrap();
    let outcome = compress_file(&path);
    // Restore write so the dir can be removed.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
    assert!(
      matches!(
        outcome,
        Ok(Outcome::Skipped {
          reason: SkipReason::PermissionDenied
        })
      ),
      "read-only → Skipped(PermissionDenied), got {outcome:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
  }
}
