// Copyright © 2026 Jalapeno Labs

import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    include: [ 'src/**/*.test.ts', 'tests/**/*.test.ts' ],

    // Files run in parallel. Every satellite the integration suite starts takes
    // a port of its own through `ARSOX_PORT`, and the one shared resource left
    // is the cargo target directory, which cargo locks itself.

    // Long enough for `cargo build` on a cold target directory. Unit tests never
    // approach it; the integration suite's first hook can.
    hookTimeout: 600_000,
    testTimeout: 120_000
  }
})
