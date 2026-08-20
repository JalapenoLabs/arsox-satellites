# Copyright © 2026 Jalapeno Labs

"""Starts a real satellite for the integration suite.

Not a fake, not a mock of the HTTP layer. The point of these tests is that the
Python SDK drives the same binary a consumer would run, so anything the SDK needs
and cannot reach is a hole in the SDK rather than something a test double papers
over.

The satellite runs with the `test-util` fake harness, which replays a recorded
Claude transcript instead of calling a model. No network, no token budget, and a
deterministic turn.

# The port

Every satellite this module starts gets a port of its own through `ARSOX_PORT`,
so two of them coexist and the suite never has to be told which machine is free.
Bind port 0, read what the kernel handed out, release it, and give it to the
satellite. There is a race in that gap and it is worth naming: another process
could take the port before the satellite binds it. The window is milliseconds and
the failure is loud, since a satellite that cannot bind exits and its stderr is
reported here. A fixed port collides every time two satellites run rather than
almost never.
"""

from __future__ import annotations

import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections.abc import AsyncIterator, Iterator
from contextlib import suppress
from pathlib import Path

import pytest

from arsox_sdk import ArsoxError, Duration, Satellite, ThreadCreated, ThreadSettings

SECRET = "python-sdk-test-secret"

# How long the satellite gets to answer /healthz before the suite gives up.
READY_TIMEOUT_SECONDS = 60.0

# How long a killed satellite gets to exit before its files are removed anyway.
EXIT_TIMEOUT_SECONDS = 5.0

PACKAGE_ROOT = Path(__file__).resolve().parent.parent.parent
REPOSITORY_ROOT = PACKAGE_ROOT.parent.parent

EXECUTABLE_SUFFIX = ".exe" if sys.platform == "win32" else ""
SATELLITE_BINARY = REPOSITORY_ROOT / "target" / "debug" / f"arsox-satellite{EXECUTABLE_SUFFIX}"
FAKE_HARNESS_BINARY = REPOSITORY_ROOT / "target" / "debug" / f"arsox-fake-harness{EXECUTABLE_SUFFIX}"

# The recorded transcript the stand-in replays.
#
# Lives in arsox-harness because it is evidence about Claude's output rather than
# about the satellite, and the mapper's own conformance tests assert against the
# same bytes.
TRANSCRIPT = (
    REPOSITORY_ROOT / "crates" / "arsox-harness" / "fixtures" / "claude" / "2.1.221"
    / "tool-call.stdout.jsonl"
)


@pytest.fixture(scope="session")
def secret() -> str:
    """The secret every client in this suite authenticates with."""
    return SECRET


@pytest.fixture
def settings() -> ThreadSettings:
    """The settings a thread must declare: an idle TTL and a budget.

    Both are required, always. The TTL is the safety net against a forgotten
    workspace filling a disk, and a required budget is what makes an unbounded
    spend a decision rather than a field somebody left unset.
    """
    return ThreadSettings(idle_ttl=Duration(seconds=3600), budget={})


def build_satellite() -> None:
    """Build the satellite and the fake harness if they are not already there.

    Building here rather than in a separate step means `pytest` works from a
    clean checkout, which is the only version of "the tests pass" worth having.
    """
    if SATELLITE_BINARY.exists() and FAKE_HARNESS_BINARY.exists():
        return

    subprocess.run(
        [ "cargo", "build", "-p", "arsox-satellite", "--features", "test-util" ],
        cwd=REPOSITORY_ROOT,
        check=True,
    )


def reserve_ephemeral_port() -> int:
    """Ask the kernel for a port nothing is listening on.

    Probed on the interface the satellite binds, so a port free only on loopback
    is never mistaken for a port free everywhere.
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("0.0.0.0", 0))

        return int(probe.getsockname()[1])


def is_serving(url: str) -> bool:
    """Ask the satellite's unauthenticated liveness endpoint whether it is up."""
    try:
        with urllib.request.urlopen(f"{url}/healthz", timeout=1) as response:
            return bool(response.status == 200)
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return False


@pytest.fixture(scope="session")
def satellite_url() -> Iterator[str]:
    """Start a satellite on a port of its own and wait for it to answer.

    The workspace and the database land in a scratch directory that goes with the
    process, so nothing a test does survives into the next run.
    """
    build_satellite()

    port = reserve_ephemeral_port()
    url = f"http://127.0.0.1:{port}"
    scratch = Path(tempfile.mkdtemp(prefix="arsox-python-sdk-"))

    # Onto a file rather than a pipe. A satellite logs through a whole turn, and
    # a pipe nobody drains blocks the writer once its buffer fills, which reads
    # as a satellite that mysteriously stopped working mid-test.
    log = scratch / "satellite.log"
    log_handle = log.open("w", encoding="utf-8")

    satellite = subprocess.Popen(
        [ str(SATELLITE_BINARY) ],
        cwd=REPOSITORY_ROOT,
        stdout=log_handle,
        stderr=subprocess.STDOUT,
        env={
            **os.environ,
            "ARSOX_SECRET": SECRET,
            "ARSOX_PORT": str(port),
            "ARSOX_DB_PATH": str(scratch / "arsox.db"),
            "ARSOX_WORKSPACE_ROOT": str(scratch),
            "ARSOX_MAX_CONCURRENT_THREADS": "2",
            # Long enough that no test races the collector.
            "ARSOX_COLLECT_INTERVAL": "3600",
            # The stand-in harness, replaying a recorded transcript in place of a
            # CLI.
            "ARSOX_CLAUDE_BIN": str(FAKE_HARNESS_BINARY),
            "ARSOX_FAKE_TRANSCRIPT": str(TRANSCRIPT),
        },
    )

    try:
        deadline = time.monotonic() + READY_TIMEOUT_SECONDS
        while time.monotonic() < deadline:
            if satellite.poll() is not None:
                # A satellite that refuses to boot says why in its log, and a
                # suite that swallowed it would report only a timeout.
                raise RuntimeError(
                    f"the satellite exited with {satellite.returncode}:\n"
                    f"{log.read_text(encoding='utf-8')}"
                )

            if is_serving(url):
                break

            time.sleep(0.1)
        else:
            raise RuntimeError(
                f"the satellite did not answer within {READY_TIMEOUT_SECONDS}s:\n"
                f"{log.read_text(encoding='utf-8')}"
            )

        yield url
    finally:
        if satellite.poll() is None:
            satellite.terminate()
            # Waited for rather than assumed. The satellite holds its database
            # open, and removing the scratch directory out from under a live
            # process fails outright on Windows.
            with suppress(subprocess.TimeoutExpired):
                satellite.wait(timeout=EXIT_TIMEOUT_SECONDS)

        log_handle.close()
        shutil.rmtree(scratch, ignore_errors=True)


@pytest.fixture
async def client(satellite_url: str) -> AsyncIterator[Satellite]:
    """Connect a client, and release its pool when the test is done.

    Function scoped because the client holds an aiohttp session bound to the loop
    that created it, and the suite runs a loop per test.
    """
    satellite = await Satellite.connect(satellite_url, SECRET)
    try:
        yield satellite
    finally:
        await satellite.close()


@pytest.fixture
async def thread(client: Satellite, settings: ThreadSettings) -> AsyncIterator[ThreadCreated]:
    """Open a thread, and destroy it however the test ends.

    A leaked thread would hold one of the satellite's two concurrency slots for
    the rest of the session, so the next test would fail for a reason that has
    nothing to do with it.
    """
    created = await client.threads().create(settings)
    try:
        yield created
    finally:
        # A test that destroyed the thread itself has done nothing wrong.
        with suppress(ArsoxError):
            await created.handle.destroy()
