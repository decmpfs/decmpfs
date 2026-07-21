#![no_main]
//! FUZZ target `gate_glob` — the install-gate glob matcher on an attacker-influenced
//! path (`decmpfs::Gate::matches`).
//!
//! At install time the PM helper calls `gate.matches(path, len)` where `path` is a
//! `node_modules` entry whose segments carry DOWNLOADED package names — so the text
//! side of the recursive `*`/`**`/`?` matcher is attacker-influenced. The GLOB
//! PATTERN is trusted local config, so this target pins the fleet-realistic pattern
//! set and fuzzes the path text: it models the real threat (a crafted path) without
//! synthesizing adversarial patterns, whose algorithmic-complexity search belongs to
//! a dedicated campaign, not this correctness lane.
//!
//! Feed RAW bytes as the path (lossy `&str` — a real path is OS bytes). Finding =
//! panic / abort / stack overflow / hang; any `bool` answer is a NON-finding.

use decmpfs::Gate;
use libfuzzer_sys::fuzz_target;

/// The trusted, fleet-realistic patterns a manifest actually configures.
const PATTERNS: &[&str] = &[
  "**/*.node",
  "*.node",
  "**",
  "*",
  "a/**/b",
  "**/build/Release/*.node",
  "?",
];

fuzz_target!(|data: &[u8]| {
  let text = String::from_utf8_lossy(data);
  let len = data.len() as u64;
  for pat in PATTERNS {
    // `Gate::new(Some(pat), None)` cannot fail (no size predicate) and routes
    // straight into the recursive `glob_match` over the untrusted `text`.
    if let Ok(gate) = Gate::new(Some(pat), None) {
      let _ = gate.matches(&text, len);
    }
  }
});
