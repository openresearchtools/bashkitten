#!/usr/bin/env python3
"""Prepare development-only imports without hydrating Pi's external catalogs.

The fixture always passes compact() its own model and summary stream. The
unreached completeSimple() fallback is the only replaced export. Every tested
algorithm, retry classifier, prompt, context builder, and summary generator is
loaded directly from the pinned, unmodified Pi source.
"""
import json
import os
from pathlib import Path
import subprocess

root = Path(os.environ["PI_REFERENCE"]).resolve()
pin = "9841914c71a74d81abe07f751aefd271fd924e63"
assert subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip() == pin
(root / "fixture-compat.ts").write_text('export function completeSimple() { throw new Error("Fixture must supply its own summary stream"); }\n')
config = json.loads((root / "tsconfig.json").read_text())
config["compilerOptions"]["paths"]["@earendil-works/pi-ai/compat"] = ["./fixture-compat.ts"]
(root / "tsconfig.fixture.json").write_text(json.dumps(config, indent=2) + "\n")
