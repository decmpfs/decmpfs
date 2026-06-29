// Generate the per-triple npm package directories under node/npm/<triple>/, one
// per TARGETS entry: a manifest gated by os/cpu/libc that ships only that
// platform's `.node`. Idempotent codegen — the publish workflow runs this on each
// matrix host, then copies the freshly built binary into its matching directory
// (this script also does that copy locally when a host build is present).

import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { TARGETS } from './targets.mts'

const nodeRoot = join(dirname(fileURLToPath(import.meta.url)), '..')
const mainManifest = JSON.parse(readFileSync(join(nodeRoot, 'package.json'), 'utf8'))

function hostTriple(): string {
  const { arch, platform } = process
  if (platform === 'win32') {
    return `${platform}-${arch}-msvc`
  }
  if (platform === 'linux') {
    const report = process.report?.getReport()
    const glibc =
      report && typeof report === 'object' ? report.header?.glibcVersionRuntime : undefined
    return `${platform}-${arch}${glibc ? '-gnu' : '-musl'}`
  }
  return `${platform}-${arch}`
}

const host = hostTriple()

for (const target of TARGETS) {
  const dir = join(nodeRoot, 'npm', target.triple)
  mkdirSync(dir, { recursive: true })

  const nodeFile = `decmpfs.${target.triple}.node`
  const manifest = {
    name: `@jdalton/decmpfs-${target.triple}`,
    version: mainManifest.version,
    description: `decmpfs prebuilt binary for ${target.triple}.`,
    license: mainManifest.license,
    repository: mainManifest.repository,
    engines: mainManifest.engines,
    os: [target.os],
    cpu: [target.cpu],
    ...(target.libc ? { libc: [target.libc] } : {}),
    main: nodeFile,
    files: [nodeFile],
    publishConfig: mainManifest.publishConfig,
  }
  writeFileSync(join(dir, 'package.json'), `${JSON.stringify(manifest, undefined, 2)}\n`)

  // On the matrix host, stage the binary cargo just built into its dir.
  if (target.triple === host) {
    const built = join(nodeRoot, 'target', 'release', target.artifact)
    if (existsSync(built)) {
      copyFileSync(built, join(dir, nodeFile))
    }
  }
}
