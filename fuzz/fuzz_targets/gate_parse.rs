#![no_main]
//! FUZZ target `gate_parse` — the install-gate size-predicate string parser
//! (`decmpfs::SizePredicate::parse` / `decmpfs::Gate::new`).
//!
//! A manifest carries the gate verbatim (`compress = ">= 1MB"`), so the operator
//! (`>`/`>=`) + number + unit string reaches the parser as untrusted text. The
//! parse hand-splits a digit run from a unit suffix, strips digit separators,
//! `str::parse`s a `u64`, then `checked_mul`s by a unit multiplier — offset math +
//! overflow handling on arbitrary bytes.
//!
//! Feed RAW bytes decoded lossily to a `&str` (the parser takes `&str`, and the
//! lossy decode still exercises every ASCII operator/digit/unit path the real CLI
//! sees). Drive both the bare predicate parser and `Gate::new`, which also runs the
//! glob half. Finding = panic / abort / overflow; a graceful `Err(GateParseError)`
//! is a NON-finding.

use decmpfs::{Gate, SizePredicate};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
  let spec = String::from_utf8_lossy(data);
  let _ = SizePredicate::parse(&spec);
  // `Gate::new` re-runs the size parse and stores the glob unvalidated; exercise
  // the same string as both halves so the constructor's error plumbing is covered.
  let _ = Gate::new(Some(&spec), Some(&spec));
});
