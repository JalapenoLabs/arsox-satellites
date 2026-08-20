// Copyright © 2026 Jalapeno Labs

/**
 * Asserts that the npm tarball carries exactly what it should, before publish.
 *
 * `npm publish` is the one irreversible step in the release: a version number is
 * spent the moment it lands and can never be republished with different bytes.
 * So the tarball is inspected while a failure still costs nothing.
 *
 * Two things are checked, and they fail in opposite directions:
 *
 * 1. Nothing extra ships. The `files` allowlist should hold `src/`, the test
 *    suite, and any scratch file a run left behind out of the tarball. An
 *    allowlist is only worth having if something proves it is still doing its
 *    job.
 * 2. The generated contract does ship, from inside `dist`. `sync-proto` copies
 *    `gen/ts` into `src/proto` on every build, and npm cannot publish a file
 *    above the package root, so a contract that failed to compile into `dist`
 *    would produce a package that installs and then cannot resolve a single
 *    protobuf type.
 *
 * Run it from anywhere: `node .github/scripts/verify-npm-tarball.mjs`.
 */

import { execFileSync } from 'node:child_process'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

// package.json, and the README npm shows on the package page, are included
// whatever `files` says. Everything else has to be under dist.
const allowedOutsideDist = [ 'package.json', 'README.md' ]

const packageDirectory = join(dirname(dirname(dirname(fileURLToPath(import.meta.url)))), 'sdks', 'node')

// `--dry-run` builds the file list without writing a tarball, and `--json` is
// the only output of `npm pack` that is a contract rather than a log format.
// The shell is only for Windows, where npm is a .cmd shim that Node refuses to
// spawn directly. Every argument here is a literal, so there is nothing to
// escape. CI runs on Linux and takes the direct path.
const packOutput = execFileSync('npm', [ 'pack', '--dry-run', '--json' ], {
  cwd: packageDirectory,
  encoding: 'utf8',
  shell: process.platform === 'win32'
})

const [ tarball ] = JSON.parse(packOutput)
const paths = tarball.files.map((file) => file.path)

const unexpected = paths.filter((path) => !path.startsWith('dist/') && !allowedOutsideDist.includes(path))
const contractFiles = paths.filter((path) => path.startsWith('dist/proto/arsox/'))

console.log(`${tarball.name}@${tarball.version}: ${paths.length} files, ${tarball.size} bytes`)
console.log(`  generated contract files under dist/proto: ${contractFiles.length}`)

if (unexpected.length) {
  // A widened allowlist can drag in a hundred files, and a hundred paths in a
  // failure log is a wall nobody reads. Ten is enough to recognize the mistake.
  console.error(`The tarball carries ${unexpected.length} files it should not:`)
  for (const path of unexpected.slice(0, 10)) {
    console.error(`  ${path}`)
  }
  if (unexpected.length > 10) {
    console.error(`  and ${unexpected.length - 10} more`)
  }
  console.error('Fix the "files" allowlist in sdks/node/package.json rather than deleting the files.')
  process.exit(1)
}

if (!contractFiles.length) {
  console.error('The tarball carries no generated contract under dist/proto/arsox/.')
  console.error('Run `yarn build` in sdks/node, which syncs gen/ts and compiles it into dist.')
  process.exit(1)
}

console.log('The tarball carries exactly dist, the README, and package.json.')
