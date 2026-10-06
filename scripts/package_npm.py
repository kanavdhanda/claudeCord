#!/usr/bin/env python3
"""Builds the ONE npm package, `claudecord`: the launcher (npm/claudecord/bin/claudecord.js) and, next to it, the program for each
platform that was built, in bin/<platform>-<cpu>/. The launcher runs the one that fits the machine. There are no other packages to
publish, so a token for `claudecord` alone is enough. The version always comes from Cargo.toml, so Rust, npm and pip can never disagree.

    python3 scripts/package_npm.py BINARIES_DIR OUT_DIR

BINARIES_DIR holds files named claudecord-linux-x64, claudecord-linux-arm64, claudecord-macos-x64, claudecord-macos-arm64 and
claudecord-windows-x64.exe (what the release build makes). Platforms with no file are skipped, so a partial build still works.
Then `npm publish` OUT_DIR/claudecord (scripts/publish_npm.sh does that)."""
import json
import os
import re
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# npm platform, npm cpu, release file name, program name inside the package
PLATFORMS = [
    ("linux", "x64", "claudecord-linux-x64", "claudecord"),
    ("linux", "arm64", "claudecord-linux-arm64", "claudecord"),
    ("darwin", "x64", "claudecord-macos-x64", "claudecord"),
    ("darwin", "arm64", "claudecord-macos-arm64", "claudecord"),
    ("win32", "x64", "claudecord-windows-x64.exe", "claudecord.exe"),
]


def cargo_version():
    m = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.M)
    assert m, "no version in Cargo.toml"
    return m.group(1)


def build(binaries, out):
    version = cargo_version()
    binaries, out = Path(binaries), Path(out)
    shutil.rmtree(out, ignore_errors=True)
    main = out / "claudecord"
    shutil.copytree(ROOT / "npm" / "claudecord", main)
    made = []
    for plat, cpu, src_name, exe in PLATFORMS:
        src = binaries / src_name
        if not src.exists():
            continue
        d = main / "bin" / f"{plat}-{cpu}"
        d.mkdir(parents=True)
        shutil.copy(src, d / exe)
        os.chmod(d / exe, 0o755)
        made.append(f"{plat}-{cpu}")
    shutil.copy(ROOT / "README.md", main / "README.md")
    pkg = json.loads((main / "package.json").read_text())
    pkg["version"] = version
    (main / "package.json").write_text(json.dumps(pkg, indent=2) + "\n")
    return version, made


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    v, made = build(sys.argv[1], sys.argv[2])
    print(f"claudecord {v}: one package holding {len(made)} platform program(s): {', '.join(made)}")
