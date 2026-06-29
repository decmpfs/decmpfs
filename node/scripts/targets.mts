// Canonical list of platforms we ship a prebuilt `.node` for — the single source
// of truth for the loader's package names, the main package's optionalDependencies,
// and the per-triple npm directories the publish workflow builds. Triples follow
// the napi-rs naming (`<platform>-<arch>[-<abi>]`).

export interface Target {
  triple: string
  os: string
  cpu: string
  // Present only where npm needs to disambiguate the C library (Linux).
  libc?: string
  // The cdylib basename cargo emits on this target's native host.
  artifact: string
}

export const TARGETS: Target[] = [
  { triple: 'darwin-arm64', os: 'darwin', cpu: 'arm64', artifact: 'libdecmpfs_node.dylib' },
  { triple: 'darwin-x64', os: 'darwin', cpu: 'x64', artifact: 'libdecmpfs_node.dylib' },
  {
    triple: 'linux-arm64-gnu',
    os: 'linux',
    cpu: 'arm64',
    libc: 'glibc',
    artifact: 'libdecmpfs_node.so',
  },
  {
    triple: 'linux-x64-gnu',
    os: 'linux',
    cpu: 'x64',
    libc: 'glibc',
    artifact: 'libdecmpfs_node.so',
  },
  { triple: 'win32-x64-msvc', os: 'win32', cpu: 'x64', artifact: 'decmpfs_node.dll' },
]
