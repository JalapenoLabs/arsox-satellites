// Copyright © 2026 Jalapeno Labs

import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    include: [ 'src/**/*.test.ts', 'tests/**/*.test.ts' ],

    // The integration suite boots a real satellite, which binds a fixed port and
    // builds a Rust binary on its first run. Both are process-wide resources, so
    // test files run one at a time rather than racing each other for them.
    fileParallelism: false,

    // Long enough for `cargo build` on a cold target directory. Unit tests never
    // approach it; the integration suite's first hook can.
    hookTimeout: 600_000,
    testTimeout: 120_000
  }
})
