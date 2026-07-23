// No-network, no-fs unit tests for the pure release resolver. Run: node --test.
import assert from 'node:assert/strict'
import { test } from 'node:test'

import { resolveRelease } from './release-lib.mts'

void test('resolveRelease: prerelease hint finalizes, arg bumps, else as-committed', () => {
  assert.deepEqual(resolveRelease('0.1.1-prerelease', ''), {
    version: '0.1.1',
    mode: 'finalize',
  })
  assert.deepEqual(resolveRelease('0.1.0', '0.2.0'), {
    version: '0.2.0',
    mode: 'bump',
  })
  assert.deepEqual(resolveRelease('0.1.0', ''), {
    version: '0.1.0',
    mode: 'as-committed',
  })
  // An arg equal to the current version is not a bump.
  assert.deepEqual(resolveRelease('0.1.0', '0.1.0'), {
    version: '0.1.0',
    mode: 'as-committed',
  })
  // A build/other suffix also finalizes.
  assert.deepEqual(resolveRelease('2.3.4-rc.1', ''), {
    version: '2.3.4',
    mode: 'finalize',
  })
})
