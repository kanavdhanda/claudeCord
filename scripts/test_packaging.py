#!/usr/bin/env python3
"""Proves the packages work the way a person gets them: builds the real program, makes the npm packages and the pip wheel,
installs each into a clean folder, and runs `claudecord --help` through the installed command. Also checks the version agrees
everywhere and the launcher's platform choice and error messages.
    python3 scripts/test_packaging.py [--skip-pip]"""
import json
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
import package_npm  # noqa: E402


def run(*cmd, cwd=None):
    # On Windows npm and uv are .cmd or .exe files that only a full path finds.
    cmd = (shutil.which(cmd[0]) or cmd[0], *cmd[1:])
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    assert r.returncode == 0, f"{' '.join(map(str, cmd))} failed:\n{r.stdout}\n{r.stderr}"
    return r.stdout


def this_platform():
    plat = {"Darwin": "macos", "Linux": "linux", "Windows": "windows"}[platform.system()]
    cpu = "arm64" if platform.machine().lower() in ("arm64", "aarch64") else "x64"
    return plat, cpu


def test_npm(binary, tmp):
    plat, cpu = this_platform()
    bins = tmp / "bins"
    bins.mkdir()
    shutil.copy(binary, bins / f"claudecord-{plat}-{cpu}{'.exe' if plat == 'windows' else ''}")
    version, made = package_npm.build(bins, tmp / "npm")
    assert version == package_npm.cargo_version() and len(made) == 1, made
    assert sorted(p.name for p in (tmp / "npm").iterdir()) == ["claudecord"], "only one package is made"
    main = json.loads((tmp / "npm/claudecord/package.json").read_text())
    assert main["version"] == version and "optionalDependencies" not in main
    tars = tmp / "tars"
    tars.mkdir()
    run("npm", "pack", str(tmp / "npm" / "claudecord"), "--pack-destination", str(tars))
    site = tmp / "site"
    site.mkdir()
    run("npm", "init", "-y", cwd=site)
    run("npm", "install", "--no-audit", "--no-fund", *map(str, sorted(tars.glob("*.tgz"))), cwd=site)
    launcher = site / "node_modules/.bin" / ("claudecord.cmd" if platform.system() == "Windows" else "claudecord")
    out = run(str(launcher), "--help")
    assert "hub" in out and "start" in out, out
    # The exit code of the program comes through the launcher.
    bad = subprocess.run([str(launcher), "no-such-command"], capture_output=True)
    assert bad.returncode != 0
    print("npm: installed the one package from its tarball and ran claudecord --help")


def test_launcher():
    script = """
const { find, KEYS } = require(%r);
const no = () => false, yes = () => true;
if (find('linux', 'x64', { CLAUDECORD_BINARY: '/x/y' }, '/d', no) !== '/x/y') throw new Error('override ignored');
if (!(find('freebsd', 'x64', {}, '/d', yes) instanceof Error)) throw new Error('unknown platform accepted');
if (!/missing from this install/.test(find('linux', 'x64', {}, '/d', no).message)) throw new Error('missing program message');
const p = find('win32', 'x64', {}, '/d', yes);
if (!p.endsWith('claudecord.exe') || !p.includes('win32-x64')) throw new Error('windows path ' + p);
const m = find('darwin', 'arm64', {}, '/d', yes);
if (!m.endsWith('claudecord') || !m.includes('darwin-arm64')) throw new Error('mac path ' + m);
if (KEYS.length !== 5) throw new Error('five platforms');
""" % str(ROOT / "scripts/npm/bin/claudecord.js")
    run("node", "-e", script)
    print("npm launcher: platform choice, override and error messages")


def test_pip(tmp):
    wheels = tmp / "wheels"
    run("uvx", "maturin", "build", "--release", "--out", str(wheels), cwd=ROOT)
    wheel = next(wheels.glob("claudecord-*.whl"))
    assert package_npm.cargo_version() in wheel.name, wheel.name
    venv = tmp / "venv"
    run("uv", "venv", str(venv))
    py = venv / ("Scripts/python.exe" if platform.system() == "Windows" else "bin/python")
    run("uv", "pip", "install", "--python", str(py), str(wheel))
    cmd = venv / ("Scripts/claudecord.exe" if platform.system() == "Windows" else "bin/claudecord")
    out = run(str(cmd), "--help")
    assert "hub" in out and "start" in out, out
    print(f"pip: built {wheel.name}, installed into a clean environment and ran claudecord --help")


if __name__ == "__main__":
    tmp = Path(tempfile.mkdtemp(prefix="cc-pack-"))
    try:
        run("cargo", "build", "--release", "--bin", "claudecord", cwd=ROOT)
        exe = "claudecord.exe" if platform.system() == "Windows" else "claudecord"
        test_launcher()
        test_npm(ROOT / "target/release" / exe, tmp)
        if "--skip-pip" not in sys.argv:
            test_pip(tmp)
        cargo = package_npm.cargo_version()
        assert re.search(r'dynamic\s*=\s*\["version"\]', (ROOT / "pyproject.toml").read_text()), "pip takes its version from Cargo.toml"
        print(f"packaging ok at version {cargo}")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
