#!/usr/bin/env python3
"""Expose a bounded Android build failure in GitHub's Checks API."""
from pathlib import Path
import sys


title, log = sys.argv[1:]
# cargo-ndk panic reports may append runner environment variables.
lines = Path(log).read_text(errors="replace").split("\nenv:\n", 1)[0].splitlines()
details = "\n".join(lines[-100:])[-12000:]
details = details.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
print(f"::error title={title}::{details}")
