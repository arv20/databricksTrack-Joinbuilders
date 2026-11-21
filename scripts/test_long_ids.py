#!/usr/bin/env python3
"""Run the hand-written fixtures with long UTF-8 event, user and click IDs.

This exercises borrowed identities, byte ordering and name ownership after mmap
release. The same order-preserving replacement is applied to expected tables.
Usage: python scripts/test_long_ids.py -- rust/target/release/rproto
"""
from pathlib import Path
import re
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parents[1]
command = sys.argv[2:] if sys.argv[1:2] == ["--"] else []
if not command:
    raise SystemExit(__doc__)
prefix = "long-identity-\u65e5\u672c-".encode()
with tempfile.TemporaryDirectory(prefix="sessions-long-ids-") as tmp:
    destination = Path(tmp)
    for path in (root / "fixtures").rglob("*"):
        if not path.is_file():
            continue
        target = destination / path.relative_to(root / "fixtures")
        target.parent.mkdir(parents=True, exist_ok=True)
        content = path.read_bytes()
        if path.suffix == ".csv":
            content = re.sub(rb"[uUeEkK][0-9]+", lambda m: prefix + m[0], content)
        target.write_bytes(content)
    raise SystemExit(subprocess.call([
        sys.executable, str(root / "scripts/run_fixtures.py"), str(destination), "--", *command
    ]))
