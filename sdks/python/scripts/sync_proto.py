# Copyright © 2026 Jalapeno Labs

"""Copies the generated protobuf contract into the package.

`gen/python` at the repository root is the one source of truth for the Python
contract, emitted by `buf generate` and committed. It sits outside this package,
and a wheel can only carry files beneath its own package directory, so the
published distribution has to hold its own copy. This is the same constraint that
puts the TypeScript copy in `sdks/node/src/proto` and the Rust output inside
`arsox-sdk` rather than under `gen/`.

Copying rather than committing a second copy keeps `gen/python` the only
checked-in generated tree, so the contract can never be current in one place and
stale in the other. `src/arsox_sdk/proto` is git ignored and rebuilt on every
build, typecheck, and test run. Nothing reads a `.proto` at runtime.

# The one rewrite

The generated modules import each other by absolute path: `arsox.error.v1`
imports `arsox.common.v1`. That resolves when `gen/python` is the import root and
nowhere else, so a plain copy into `arsox_sdk` would import a top-level `arsox`
package this distribution does not ship. The alternative to rewriting is
claiming the top-level `arsox` name for generated code, which squats the most
useful import name in the ecosystem on machine-written modules.

So the copy rewrites exactly one thing, the import prefix, mechanically and on
every sync. Nothing else about the generated files is touched.
"""

from __future__ import annotations

import re
import shutil
from pathlib import Path

PACKAGE_ROOT = Path(__file__).resolve().parent.parent
REPOSITORY_ROOT = PACKAGE_ROOT.parent.parent

SOURCE = REPOSITORY_ROOT / "gen" / "python" / "arsox"
PROTO_ROOT = PACKAGE_ROOT / "src" / "arsox_sdk" / "proto"
DESTINATION = PROTO_ROOT / "arsox"

# Matches the generated cross-package import, and only at the start of a line so
# the string never appears inside a serialized descriptor.
CONTRACT_IMPORT = re.compile(r"^from arsox\.", flags=re.MULTILINE)

PROTO_INIT = '''# Copyright © 2026 Jalapeno Labs

"""The generated protobuf contract, synced from `gen/python`.

Written by scripts/sync_proto.py. Nothing here is edited by hand and nothing here
is committed: change the `.proto` files and regenerate.
"""
'''


def main() -> None:
    """Sync the generated contract into the package source."""
    if not SOURCE.is_dir():
        raise FileNotFoundError(f"the generated contract is not at {SOURCE}")

    # Removed first rather than copied over, so a message deleted upstream does
    # not linger here and keep importing.
    shutil.rmtree(PROTO_ROOT, ignore_errors=True)
    PROTO_ROOT.mkdir(parents=True)
    (PROTO_ROOT / "__init__.py").write_text(PROTO_INIT, encoding="utf-8")

    shutil.copytree(SOURCE, DESTINATION, ignore=shutil.ignore_patterns("__pycache__"))

    for generated in [ *DESTINATION.rglob("*.py"), *DESTINATION.rglob("*.pyi") ]:
        source = generated.read_text(encoding="utf-8")
        generated.write_text(
            CONTRACT_IMPORT.sub("from arsox_sdk.proto.arsox.", source),
            encoding="utf-8",
        )

    print(f"synced the protobuf contract into {DESTINATION}")


if __name__ == "__main__":
    main()
