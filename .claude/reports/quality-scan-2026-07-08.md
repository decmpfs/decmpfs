# decmpfs code-quality scan — 2026-07-08

Scope: decmpfs Rust core (`crates/decmpfs`) + napi addon (`napi/decmpfs`) + build scripts (`scripts/`, `.github/workflows/`).

## Grade: C

At least one High-severity defect (three, all cross-platform correctness gaps) but no Critical or systemic unsafety.

## Counts

- Critical: 0
- High: 3
- Medium: 5
- Low: 3

## High

### Windows `detect()` can't open a directory handle, so `compress_bytes` silently never compresses fresh installs
`crates/decmpfs/src/windows.rs:73` (verified)

A package manager calls `compress_bytes(new_addon_path, bytes, gate)` for a `.node` file being freshly written during install on NTFS. The target doesn't exist yet, so `compress_bytes_with` probes the parent directory; `open_with()` calls `CreateFileW` with `dwFlagsAndAttributes = 0`, which cannot open a directory handle and returns `ERROR_ACCESS_DENIED`. `Err(_)` shares the match arm with `Unsupported`, so the crate writes the file plain and reports `Outcome::Unsupported { reason: Filesystem }` — every fresh Windows install skips compression and misreports the reason. macOS (statfs) and Linux (statfs) work fine against a directory path.

Fix idea: pass `FILE_FLAG_BACKUP_SEMANTICS` (0x0200_0000) in the `detect()`/`open_with` call, or detect via `GetFileAttributesW`/volume-root APIs instead of opening a handle to an arbitrary path.

Variants: the public `probe(path)` API (`lib.rs:133`) would hit the same failure on a directory; check `copy_file_with`'s Windows path, which routes through `compress_bytes_with` for a fresh destination and inherits the parent-dir probe.

### `classify_skip` only recognizes POSIX errno for Busy, so a Windows sharing/lock violation escapes as a hard `Err`
`crates/decmpfs/src/safety.rs:105` (verified)

`compress_file(path)` on Windows against a `.node` addon briefly locked by another process (antivirus scan, lingering handle during reinstall) fails with `ERROR_SHARING_VIOLATION` (32). `classify_skip` matches `raw_os_error()` against POSIX `16`/`26` (EBUSY/ETXTBSY) only — 32 matches neither — so it returns `None` and `apply_guarded` propagates `Err(Error::Io)`. This contradicts the crate's own contract ("a permission/lock issue... are non-fatal Outcomes"); mac/Linux hit the equivalent `EBUSY`/`ETXTBSY` and skip gracefully. The crate's `remove.rs::retryable()` (remove.rs:69-73) already encodes 32/33 correctly.

Fix idea: add a `#[cfg(windows)]` arm mapping `ERROR_SHARING_VIOLATION` (32) and `ERROR_LOCK_VIOLATION` (33) to `SkipReason::Busy`, mirroring `remove.rs::retryable`.

Variants: the second `classify_skip` caller `compress_bytes_guarded` (safety.rs:120-136) has the same gap but is softened by the plain-write fallback at `lib.rs:211` (mislabeled Outcome, still non-fatal); `classify_skip` also lacks Windows analogs for its other POSIX arms (e.g. `ERROR_WRITE_PROTECT` 19).

### `build_resource_fork` emits a malformed offset table for zero-length content
`crates/decmpfs/src/macos.rs:152` (verified)

`compress_file(path)` on a pre-existing 0-byte file reads `raw = []`. `num_blocks` is `raw.len().div_ceil(BLOCK).max(1)` = 1, but the block list is built from `raw.chunks(BLOCK)`, which yields zero chunks for an empty slice. `table_len = (num_blocks + 1) * 4` seeds a 2-entry table while the write loop emits neither the second entry nor block bytes, producing a 4-byte resource fork `[8, 0, 0, 0]` — offset[0] pointing 8 bytes into a 4-byte xattr. That inconsistent blob plus `UF_COMPRESSED` is written and atomically renamed over the original. `apply_bytes` has no zero-length floor (`within_decmpfs_limit` only caps the upper bound), and `apply_inplace`/`compress_file` reach it with no `Gate`.

Fix idea: special-case `raw.is_empty()` in `build_resource_fork` (correctly-sized single-entry table for zero blocks, or skip decmpfs and write a plain empty file), plus a unit test asserting last offset == buffer length.

Variants: a 1-byte input makes `compression_encode_buffer` return 0, so `apply_bytes` hard-errors `("lzvn encode", InvalidData)` instead of skipping gracefully, and `classify_skip` doesn't map `InvalidData` to a skip reason; the rollback-bypass at `safety.rs:61` is general. macOS-only — `linux.rs`/`windows.rs` delegate to the FS codec and build no offset table.

## Medium

### errno/code table is POSIX-only; every real Windows fs error is mis-mapped
`napi/decmpfs/src/lib.rs:583`

`errno_name()` recognizes POSIX errno only, and `fs_code_errno()` feeds it `raw_os_error()` verbatim. On Windows `raw_os_error()` returns the Win32 `GetLastError()` space, which rarely coincides numerically (`ERROR_SHARING_VIOLATION`=32 vs POSIX `EBUSY`=16; `ERROR_ACCESS_DENIED`=5 vs `EACCES`=13). `rmSync` on a locked file throws `{ code: 'UNKNOWN', errno: -32 }` instead of Node's `{ code: 'EBUSY' }`, so callers written against the standard `err.code` pattern never match on Windows.

Fix idea: add a `#[cfg(windows)]` branch mapping Win32 codes to Node/libuv `code` strings, and check `source.kind()` before `raw_os_error()` (mirroring `safety.rs::classify_skip`).

### write/copy/copyFile/pack error paths never get the Node fs error shape
`napi/decmpfs/src/lib.rs:133`

`run()`, `run_copy()`, `run_copy_file()`, and `run_pack()` build errors with `Error::new(Status::GenericFailure, ...)`, which napi-rs surfaces as `.code === "GenericFailure"` with no `errno`/`syscall`/`path`. `writeDecmpfsFile('/no/such/dir/out.bin', buf)` rejects with `err.code === 'GenericFailure'` instead of `{ code: 'ENOENT', errno: -2, syscall: 'open', path }`, so standard `err.code === 'ENOENT'` handling never matches — despite the module's stated Node-fs parity. Only `rm`/`compressFile` route through `throw_fs`/`throw_decmpfs`.

Fix idea: route these paths through the existing `fs_code_errno`/`throw_fs` machinery, threading `Env` through the sync fns and async `reject()` impls.

### Unmapped IO error kinds report a misleading "UNKNOWN"/success-looking message
`napi/decmpfs/src/lib.rs:626`

`fs_code_errno`'s kind-fallback special-cases only `PermissionDenied`/`NotFound`/`AlreadyExists`; anything else returns `("UNKNOWN", 0)`. The path is reachable: `cstring()` (crates/decmpfs/src/lib.rs:124-130) and `linux.rs:196` return `ErrorKind::InvalidInput` for a path with an interior NUL. `throw_fs` then calls `node_strerror(0)`, rendering `"undefined error: 0"` (macOS) / `"success"` (glibc) — a thrown error reading `"UNKNOWN: undefined error: 0, rm '/tmp/evil\0x'"` that discards the known cause.

Fix idea: fall back on the `decmpfs::Error`'s own `context` string when the `ErrorKind` isn't mapped, instead of feeding errno 0 through `node_strerror`.

### The `addon` feature's unit tests never execute in CI or pre-push
`scripts/check.mts:47`

`check.mts` runs only `cargo test --features exe`; there is no `cargo test --features addon` anywhere (check.mts, ci.yml, publish-npm.yml, pre-push). `addon.rs` is `#[cfg(feature = "addon")]`-gated with 9 `#[test]` fns covering the SHA-512 integrity check and magic/size-header parsing, but `cargo test --workspace --locked` uses default features and never compiles the module. A regression in `unwrap_if_hybrid` integrity/length math (e.g. an off-by-one letting a tampered payload pass) ships with green CI — `clippy --features addon` only lints.

Fix idea: add `run('cargo test (addon)', 'cargo', ['test', '--features', 'addon'])` alongside the existing exe test call, mirroring the `FEATURE_SETS` clippy loop; confirm `cargo-llvm-cov` also compiles `addon`.

### npm publish loop has no already-published skip, unlike the crate publish flow
`.github/workflows/publish-npm.yml:133`

The publish job runs 6 sequential `npm publish` calls under `bash -eo pipefail` with no per-package already-live check. If a middle package fails on a transient registry error, `set -e` aborts and the remaining packages never publish; re-running the job re-executes from the first package, which now 403s ("cannot publish over the previously published version"), aborting again before reaching the un-published ones — the retry mechanism can't complete the release, forcing a hand-run outside the attested workflow. `scripts/publish-crate.mts:43-59` already reads the registry and no-ops on already-published versions.

Fix idea: probe each package with `npm view <pkg>@<version> version` (or a dry-run) and skip if it resolves, matching the idempotent crate flow.

## Low

### Mach-O linkedit-pointer offsets truncate/overflow when the injected payload exceeds ~4 GiB
`crates/decmpfs/src/exe/inject.rs:425`

`inject_macho` writes the new section `offset` via `fileoff as u32` (line 263) and bumps every LINKEDIT-relative offset with `current + delta as u32` (line 425), with no validation of `body_len`/`delta` or the sums against `u32::MAX`. Packing a `src` whose zstd-compressed size nears/exceeds `u32::MAX` (plausible for this general-purpose "pack any executable" API) truncates/wraps silently in release builds (`overflow-checks = false`) — producing a corrupt Mach-O whose `LC_SYMTAB`/`LC_DYSYMTAB`/`LC_DYLD_INFO` offsets point at the wrong locations (codesign/dyld rejection or OOB mis-parse) — and panics in debug builds. The sibling decmpfs path (`macos.rs:35-37,56-64`) caps its u32 math via `MAX_RAW`/`within_decmpfs_limit`; `inject.rs` has no analog.

Fix idea: validate `body_len`/`delta` and the `fileoff`/`current + delta` sums against `u32::MAX`, returning a descriptive `Err` (EFBIG-style, like `within_decmpfs_limit`) before the `as u32` casts.

Variants: line 425 is the only payload-scaled unchecked u32 arithmetic; lines 263 and 386 operate on small stub-derived values. The ELF/PE `append_footer` path does no offset surgery.

### `restore()` silently swallows write/rename failures, so a corrupted file is reported as a benign Outcome
`crates/decmpfs/src/safety.rs:171`

`restore()` returns `()`. If the temp `File::create`/`write_all`/`sync_all` or the final `rename` fails (`ENOSPC`, dir gone read-only, another process holding the target), it does `let _ = remove_file(&tmp);` and returns silently. `verify_loadable_or_restore` (61-64) and `verify_readback_or_restore` (154-159) then unconditionally return `Ok(Skipped)`. When a backend corrupts the file and the rollback fails mid-`ENOSPC`, the caller (a package manager) sees a normal non-fatal Outcome while the file is left corrupt on disk — the exact "corrupt addon stranded" scenario the module doc says can never happen.

Fix idea: make `restore` return `Result<(), io::Error>` and propagate a rollback failure as a hard `Error::Io` instead of `Ok(Skipped)`.

Variants: the shape is centralized in the single `restore()` helper; `compress_bytes`/`copy_file` paths are already netted by the unconditional plain-write fallback at `lib.rs:209-220`/`285`.

### decmpfs temp file uses only the PID for uniqueness, so a reused-PID leftover permanently blocks re-compressing the same file
`crates/decmpfs/src/macos.rs:284`

`apply_bytes` builds `.{name}.decmpfs-{pid}.tmp` and opens with `create_new(true)`, with no pre-cleanup. A crashed/SIGKILLed prior run that left `.name.decmpfs-1234.tmp` before `rename` blocks a later run that gets PID 1234 again (common after reboot) — `create_new` fails `AlreadyExists` on every retry until a human deletes the stale file, despite the crate's retriable-by-design intent.

Fix idea: add call-scoped uniqueness beyond the PID (counter, random suffix, or thread id), and/or `let _ = remove_file(&tmp);` before the `create_new` attempt.

Variants: none — this is the only `create_new(true)` temp site; the other three temp writers (linux.rs:208, lib.rs:385, safety.rs:177) use truncating creates that overwrite a stale sibling.

## What's solid

The crate's fail-soft contract is real and mostly well-realized: the decmpfs path caps its u32 offset math with `MAX_RAW`/`within_decmpfs_limit` and documents the overflow class exactly (`macos.rs:35-64`); `remove.rs::retryable` already encodes the Windows sharing/lock codes correctly; `classify_skip` is `ErrorKind`-first before falling to errno; and rollback plus an unconditional plain-write fallback net most corruption and skip paths (`lib.rs:209-220`, `safety.rs`). `scripts/publish-crate.mts` is a clean idempotent-publish reference. The recurring theme across the graded findings is narrow — Windows/POSIX errno divergence and a couple of untested-by-default edges — not a structural weakness in the core codec design.
