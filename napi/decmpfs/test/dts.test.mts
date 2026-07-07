// Keep index.d.cts honest with real tooling, not a regex:
//
//   1. `tsc --noEmit` (TypeScript 7 native) type-checks index.d.cts against the
//      uses-api.ts fixture, which imports and uses every public export. A wrong
//      or missing declaration — or a declaration for a name the fixture can't
//      import — fails the type-check. This is the exhaustive "declarations are
//      valid and complete" gate.
//   2. A runtime check imports the public API from the built addon and asserts
//      each name is actually present (a .d.cts can declare an export the native
//      addon doesn't ship; tsc can't see that, the runtime import can).

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createRequire } from 'node:module'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'

const here = path.dirname(fileURLToPath(import.meta.url))
const pkgRoot = path.join(here, '..')
const require = createRequire(import.meta.url)

test('tsc type-checks index.d.cts against the consumer fixture', () => {
  // The typescript@7.0.1-rc bin (native tsc). Resolve via package.json (its
  // exports map doesn't expose ./bin/tsc) so the path is hoist-agnostic.
  const tsc = path.join(
    path.dirname(require.resolve('typescript/package.json')),
    'bin',
    'tsc',
  )
  execFileSync(process.execPath, [tsc, '--noEmit', '-p', 'tsconfig.json'], {
    cwd: pkgRoot,
    stdio: 'inherit',
  })
})

test('every declared public export is present on the built addon', () => {
  const addon = require('../index.cjs') as Record<string, unknown>
  const functions = [
    'copyDecmpfsFile',
    'copyDecmpfsFileSync',
    'copyFile',
    'copyFileSync',
    'packExecutable',
    'packExecutableSync',
    'writeDecmpfsFile',
    'writeDecmpfsFileSync',
  ]
  for (const name of functions) {
    assert.equal(typeof addon[name], 'function', `addon must export function ${name}`)
  }
  const consts = ['COPYFILE_EXCL', 'COPYFILE_FICLONE', 'COPYFILE_FICLONE_FORCE']
  for (const name of consts) {
    assert.equal(typeof addon[name], 'number', `addon must export const ${name}`)
  }
})
