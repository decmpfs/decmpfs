// Pure, side-effect-free helpers for the decmpfs release entry (release.mts).
// Kept importable + no-I/O so they can be unit-tested with no network and no
// filesystem — the release script wires this to real files, git, and cargo.

export interface Resolution {
  version: string
  mode: 'finalize' | 'bump' | 'as-committed'
}

// Resolve the version to release from the committed version and an optional arg:
//   - a `-prerelease` (or any `-suffix`) committed version + no arg → FINALIZE
//     to the plain semver (0.1.1-prerelease → 0.1.1), reusing the CHANGELOG
//     section already written for it (kept verbatim, never re-stubbed);
//   - a new semver arg → BUMP (insert a CHANGELOG stub to fill in);
//   - otherwise release WHAT IS COMMITTED.
export function resolveRelease(current: string, argVersion: string): Resolution {
  const pre = current.match(/^(?<base>\d+\.\d+\.\d+)-[0-9A-Za-z.-]+$/)
  if (pre && !argVersion) {
    return { version: pre.groups!['base']!, mode: 'finalize' }
  }
  if (argVersion && argVersion !== current) {
    return { version: argVersion, mode: 'bump' }
  }
  return { version: current, mode: 'as-committed' }
}
