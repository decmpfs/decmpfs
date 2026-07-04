// Build the release cdylib and stage it as the loadable `decmpfs.node` addon.
// Platform-aware: cargo emits a different artifact name per OS (a `lib` prefix on
// Unix, none on Windows), so a hardcoded `.dylib` copy only works on macOS.

import { execFileSync } from 'node:child_process'
import { copyFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

// The cdylib filename cargo writes to target/release for each platform. The Rust
// crate's lib artifact is `decmpfs_node`.
const ARTIFACT: Record<string, string | undefined> = {
  darwin: 'libdecmpfs_node.dylib',
  linux: 'libdecmpfs_node.so',
  win32: 'decmpfs_node.dll',
}

const artifact = ARTIFACT[process.platform]
if (!artifact) {
  throw new Error(
    `decmpfs build: no cdylib artifact mapping for platform "${process.platform}" — ` +
      `add it to napi/decmpfs/scripts/build.mts (expected darwin, linux, or win32).`,
  )
}

// This package is a member of the cargo workspace rooted at the repo, so cargo
// writes the cdylib to the WORKSPACE-ROOT target/, not this package's dir.
const nodeRoot = join(dirname(fileURLToPath(import.meta.url)), '..')
const repoRoot = join(nodeRoot, '..', '..')

execFileSync('cargo', ['build', '-p', 'decmpfs-node', '--release'], {
  cwd: repoRoot,
  stdio: 'inherit',
})
copyFileSync(join(repoRoot, 'target', 'release', artifact), join(nodeRoot, 'decmpfs.node'))
