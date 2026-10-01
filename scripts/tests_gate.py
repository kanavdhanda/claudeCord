#!/usr/bin/env python3
"""Checks that a code change comes with tests: reads the diff against a base and says whether source files changed
without any test being added or changed. Used by the pull request bot (.github/workflows/pr-tests.yml), and runnable by hand:
    python3 scripts/tests_gate.py [BASE]     # default origin/main
Prints a short markdown verdict; exits 1 when source changed and no test was touched. `python3 scripts/tests_gate.py --self-check`
tests the rule itself."""
import re
import subprocess
import sys

# A diff line that adds a test.
TEST_LINE = re.compile(r"^\+\s*#\[(tokio::)?test\b")


def verdict(changed, added_lines):
    """changed: list of file paths. added_lines: list of added diff lines. Returns (ok, markdown)."""
    src = [f for f in changed if f.startswith("src/") and f.endswith(".rs")]
    tests = [f for f in changed if f.startswith("tests/") or f.startswith("examples/")]
    new_tests = sum(1 for l in added_lines if TEST_LINE.match(l))
    if not src:
        return True, "No source files changed, so no new tests are needed."
    if new_tests or tests:
        return True, f"{len(src)} source file(s) changed and tests came with them ({new_tests} new test(s), {len(tests)} test file(s) touched)."
    return False, (
        f"{len(src)} source file(s) changed but no test was added or changed. Please add a test that fails without this change "
        "(see CONTRIBUTING.md), or say in the pull request why none is needed.\n\nChanged: " + ", ".join(sorted(src)[:10])
    )


def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout


def main():
    if "--self-check" in sys.argv:
        t = "+#[test]"
        assert verdict(["README.md"], [])[0]
        assert not verdict(["src/hub/core.rs"], ["+let x = 1;"])[0]
        assert verdict(["src/hub/core.rs", "tests/hub.rs"], [])[0]
        assert verdict(["src/hub/core.rs"], [t])[0]
        assert verdict(["src/hub/core.rs"], ["+    #[tokio::test]"])[0]
        print("tests_gate self-check passed")
        return 0
    base = next((a for a in sys.argv[1:] if not a.startswith("-")), "origin/main")
    changed = git("diff", "--name-only", f"{base}...HEAD").split()
    added = [l for l in git("diff", "-U0", f"{base}...HEAD").splitlines() if l.startswith("+")]
    ok, text = verdict(changed, added)
    print(("**Tests: ok.** " if ok else "**Tests: missing.** ") + text)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
