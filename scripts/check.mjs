// Full static gate: format check + clippy (the Rust "typecheck") across the
// workspace. Pre-push and CI both run this so a push never lands what CI
// would reject.
//
//   node scripts/check.mjs

import { execFileSync } from 'node:child_process'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..')

function run(label, cmd, args) {
  console.log(`check: ${label}`)
  try {
    execFileSync(cmd, args, { cwd: root, stdio: 'inherit' })
  } catch {
    console.error(`check: ${label} failed.`)
    process.exit(1)
  }
}

run('cargo fmt --check', 'cargo', ['fmt', '--all', '--check'])
run('cargo clippy', 'cargo', [
  'clippy',
  '--workspace',
  '--all-targets',
  '--locked',
  '--',
  '-D',
  'warnings',
])
run('version parity', process.execPath, [
  path.join(root, 'scripts', 'check-versions.mjs'),
])
console.log('check: all green.')
