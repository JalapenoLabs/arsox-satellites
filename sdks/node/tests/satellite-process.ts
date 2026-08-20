// Copyright © 2026 Jalapeno Labs

/**
 * Starts a real satellite for the integration suite.
 *
 * Not a fake, not a mock of the HTTP layer. The point of these tests is that the
 * Node SDK drives the same binary a consumer would run, so anything the SDK
 * needs and cannot reach is a hole in the SDK rather than something a test
 * double papers over.
 *
 * The satellite runs with the `test-util` fake harness, which replays a recorded
 * Claude transcript instead of calling a model. No network, no token budget, and
 * a deterministic turn.
 *
 * # The port
 *
 * `serve()` binds `0.0.0.0:8080` with no override, deliberately: the container's
 * port mapping is where the satellite's reachable address is decided, and a
 * second knob would only be a way for the two to disagree. That is right for the
 * satellite and it means this suite cannot pick an ephemeral port. So it runs on
 * 8080, one satellite at a time, and skips itself with a clear message when
 * something else already holds the port. CI must leave 8080 free.
 */

// Core
import { createServer } from 'node:net'
import { execFile, spawn } from 'node:child_process'
import { get } from 'node:http'
import { access, mkdtemp, rm } from 'node:fs/promises'
import { setTimeout as sleep } from 'node:timers/promises'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { tmpdir } from 'node:os'
import { promisify } from 'node:util'

/** Fixed by the satellite. See the note above. */
export const SATELLITE_PORT = 8080

export const SATELLITE_URL = `http://127.0.0.1:${SATELLITE_PORT}`

export const SECRET = 'node-sdk-test-secret'

/** How long the satellite gets to answer /healthz before the suite gives up. */
const READY_TIMEOUT_MILLISECONDS = 60_000

const packageRoot = dirname(dirname(fileURLToPath(import.meta.url)))
const repositoryRoot = join(packageRoot, '..', '..')

const executableSuffix = process.platform === 'win32'
  ? '.exe'
  : ''

const satelliteBinary = join(repositoryRoot, 'target', 'debug', `arsox-satellite${executableSuffix}`)
const fakeHarnessBinary = join(repositoryRoot, 'target', 'debug', `arsox-fake-harness${executableSuffix}`)

/**
 * The recorded transcript the stand-in replays.
 *
 * Lives in `arsox-harness` because it is evidence about Claude's output rather
 * than about the satellite, and the mapper's own conformance tests assert
 * against the same bytes.
 */
const transcript = join(
  repositoryRoot,
  'crates', 'arsox-harness', 'fixtures', 'claude', '2.1.221', 'tool-call.stdout.jsonl'
)

/** A satellite this suite started, and the way to stop it. */
export type RunningSatellite = {
  url: string
  stop: () => Promise<void>
}

/** Whether anything already holds the satellite's fixed port. */
export function portIsFree(): Promise<boolean> {
  return new Promise((resolve) => {
    const probe = createServer()

    probe.once('error', () => resolve(false))
    probe.once('listening', () => probe.close(() => resolve(true)))
    probe.listen(SATELLITE_PORT, '0.0.0.0')
  })
}

/**
 * Builds the satellite and the fake harness if they are not already there.
 *
 * Building here rather than in a separate step means `yarn test` works from a
 * clean checkout, which is the only version of "the tests pass" worth having.
 */
export async function buildSatellite(): Promise<void> {
  const built = await Promise.all([ satelliteBinary, fakeHarnessBinary ].map(
    (binary) => access(binary).then(() => true, () => false)
  ))
  if (built.every(Boolean)) {
    return
  }

  await promisify(execFile)(
    'cargo',
    [ 'build', '-p', 'arsox-satellite', '--features', 'test-util' ],
    { cwd: repositoryRoot }
  )
}

/**
 * Starts a satellite on its fixed port and waits for it to answer.
 *
 * The workspace and the database land in a scratch directory that goes with the
 * process, so nothing a test does survives into the next run.
 */
export async function startSatellite(): Promise<RunningSatellite> {
  await buildSatellite()

  const scratch = await mkdtemp(join(tmpdir(), 'arsox-node-sdk-'))

  const child = spawn(satelliteBinary, {
    cwd: repositoryRoot,
    stdio: [ 'ignore', 'pipe', 'pipe' ],
    env: {
      ...process.env,
      ARSOX_SECRET: SECRET,
      ARSOX_DB_PATH: join(scratch, 'arsox.db'),
      ARSOX_WORKSPACE_ROOT: scratch,
      ARSOX_MAX_CONCURRENT_THREADS: '2',
      // Long enough that no test races the collector.
      ARSOX_COLLECT_INTERVAL: '3600',
      // The stand-in harness, replaying a recorded transcript in place of a CLI.
      ARSOX_CLAUDE_BIN: fakeHarnessBinary,
      ARSOX_FAKE_TRANSCRIPT: transcript
    }
  })

  // Kept rather than discarded: a satellite that refuses to boot says why on
  // stderr, and a suite that swallowed it would report only a timeout.
  const output: string[] = []
  child.stdout?.on('data', (chunk: Buffer) => output.push(chunk.toString()))
  child.stderr?.on('data', (chunk: Buffer) => output.push(chunk.toString()))

  let exited: number | null = null
  child.once('exit', (code) => {
    exited = code ?? 0
  })

  const stop = async (): Promise<void> => {
    if (exited === null) {
      child.kill()
    }
    await rm(scratch, { recursive: true, force: true, maxRetries: 5 })
  }

  const deadline = Date.now() + READY_TIMEOUT_MILLISECONDS
  while (Date.now() < deadline) {
    if (exited !== null) {
      await stop()
      throw new Error(`the satellite exited with ${exited}:\n${output.join('')}`)
    }

    if (await isServing()) {
      return { url: SATELLITE_URL, stop }
    }

    await sleep(100)
  }

  await stop()
  throw new Error(`the satellite did not answer within ${READY_TIMEOUT_MILLISECONDS}ms:\n${output.join('')}`)
}

/** Asks the satellite's unauthenticated liveness endpoint whether it is up. */
function isServing(): Promise<boolean> {
  return new Promise((resolve) => {
    const request = get(`${SATELLITE_URL}/healthz`, (response) => {
      response.resume()
      resolve(response.statusCode === 200)
    })

    request.once('error', () => resolve(false))
  })
}

/** Waits for the satellite's port to be free again, so a stop is really a stop. */
export async function waitForPortRelease(): Promise<void> {
  const deadline = Date.now() + 10_000

  while (Date.now() < deadline) {
    if (await portIsFree()) {
      return
    }
    await sleep(50)
  }

  console.warn(`arsox tests: port ${SATELLITE_PORT} is still held after the satellite was stopped`)
}
