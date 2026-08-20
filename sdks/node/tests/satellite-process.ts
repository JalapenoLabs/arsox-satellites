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
 * Every satellite this module starts gets a port of its own through
 * `ARSOX_PORT`, so two of them coexist and the suite never has to be told which
 * machine is free. The container story is untouched: an image still exposes 8080
 * and `docker run -p` still decides where a container is reachable. The override
 * is for exactly this, a run with no port mapping in front of it.
 */

// Core
import { createServer } from 'node:net'
import { execFile, spawn } from 'node:child_process'
import { get } from 'node:http'
import { access, mkdtemp, rm } from 'node:fs/promises'
import { once } from 'node:events'
import { setTimeout as sleep } from 'node:timers/promises'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { tmpdir } from 'node:os'
import { promisify } from 'node:util'

export const SECRET = 'node-sdk-test-secret'

/** How long the satellite gets to answer /healthz before the suite gives up. */
const READY_TIMEOUT_MILLISECONDS = 60_000

/** How long a killed satellite gets to exit before its files are removed anyway. */
const EXIT_TIMEOUT_MILLISECONDS = 5_000

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

/**
 * Asks the kernel for a port nothing is listening on.
 *
 * Bind port 0, read what the kernel handed out, release it, and give it to the
 * satellite. There is a race in that gap: another process could take the port
 * between the close here and the satellite's own bind.
 *
 * That is worth naming rather than hiding. The window is milliseconds, kernels
 * do not immediately reissue a port they just released, and the failure is loud:
 * a satellite that cannot bind exits, and `startSatellite` reports its stderr
 * rather than hanging. The alternative is a fixed port, which collides every
 * time two satellites run rather than almost never.
 *
 * Probed on the interface the satellite binds, so a port free only on loopback
 * is never mistaken for a port free everywhere.
 */
function reserveEphemeralPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createServer()

    probe.once('error', reject)
    probe.listen(0, '0.0.0.0', () => {
      const address = probe.address()

      if (address === null || typeof address === 'string') {
        console.debug('the probe socket reported no numeric address, got', address)
        probe.close(() => reject(new Error('the probe socket reported no port')))
        return
      }

      probe.close(() => resolve(address.port))
    })
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
 * Starts a satellite on a port of its own and waits for it to answer.
 *
 * The workspace and the database land in a scratch directory that goes with the
 * process, so nothing a test does survives into the next run.
 */
export async function startSatellite(): Promise<RunningSatellite> {
  await buildSatellite()

  const port = await reserveEphemeralPort()
  const url = `http://127.0.0.1:${port}`
  const scratch = await mkdtemp(join(tmpdir(), 'arsox-node-sdk-'))

  const child = spawn(satelliteBinary, {
    cwd: repositoryRoot,
    stdio: [ 'ignore', 'pipe', 'pipe' ],
    env: {
      ...process.env,
      ARSOX_SECRET: SECRET,
      ARSOX_PORT: String(port),
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
      // Waited for rather than assumed. The satellite holds its database open,
      // and removing the scratch directory out from under a live process fails
      // outright on Windows. Bounded, so a process that refuses to die costs a
      // slow teardown rather than a hung suite.
      await Promise.race([ once(child, 'exit'), sleep(EXIT_TIMEOUT_MILLISECONDS) ])
    }
    await rm(scratch, { recursive: true, force: true, maxRetries: 5 })
  }

  const deadline = Date.now() + READY_TIMEOUT_MILLISECONDS
  while (Date.now() < deadline) {
    if (exited !== null) {
      await stop()
      throw new Error(`the satellite exited with ${exited}:\n${output.join('')}`)
    }

    if (await isServing(url)) {
      return { url, stop }
    }

    await sleep(100)
  }

  await stop()
  throw new Error(`the satellite did not answer within ${READY_TIMEOUT_MILLISECONDS}ms:\n${output.join('')}`)
}

/** Asks the satellite's unauthenticated liveness endpoint whether it is up. */
function isServing(url: string): Promise<boolean> {
  return new Promise((resolve) => {
    const request = get(`${url}/healthz`, (response) => {
      response.resume()
      resolve(response.statusCode === 200)
    })

    request.once('error', () => resolve(false))
  })
}
