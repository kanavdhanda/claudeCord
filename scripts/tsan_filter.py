#!/usr/bin/env python3
"""Reads the output of a test run under ThreadSanitizer and says whether any race is in claudeCord's own code.

ThreadSanitizer cannot see through the async runtime and the bundled SQLite (C code built without it, and atomic fences it does not
model), so a run reports dozens of "races" that are inside tokio, hyper and SQLite. Failing on all of them would make the job always red
and prove nothing. What matters is a racing ACCESS (the first frame of each side of a report) in our own code, so that is what fails here:

    python3 scripts/tsan_filter.py tsan.log      # exit 1 if a racing access is in claudeCord's code, or a test failed
    python3 scripts/tsan_filter.py --self-check
"""
import re
import sys

OURS = re.compile(r"(<)?(claudecord|it)::")
ACCESS = re.compile(r"\s*(Read|Write|Atomic read|Atomic write|Previous (read|write|atomic read|atomic write))")


def analyse(text):
    """Returns (racing accesses, those in our own code, whether a test failed)."""
    lines = [re.sub(r"^.*?\d{4}-\d\d-\d\dT[\d:.]+Z ", "", l) for l in text.splitlines()]
    tops, expect = [], False
    for l in lines:
        if ACCESS.match(l):
            expect = True
        elif expect and re.match(r"\s*#0 ", l):
            tops.append(re.sub(r"^\s*#0\s+", "", l))
            expect = False
    mine = [f for f in tops if OURS.match(f)]
    # Only a real test failure counts: when ThreadSanitizer reports anything it also makes the test program exit non-zero, and cargo then prints
    # "error: test failed" even though every test passed.
    failed = bool(re.search(r"test result: FAILED", text))
    return tops, mine, failed


def main():
    if "--self-check" in sys.argv:
        noise = "  Atomic read of size 8 at 0x1 by thread T1:\n    #0 core::sync::atomic::atomic_load::<usize> /rustc/lib.rs:1\n    #1 <tokio::runtime::io::Driver>::turn x\n  Previous write of size 8 at 0x1 by thread T2:\n    #0 memcpy ??:?\n"
        ours = "  Write of size 8 at 0x1 by thread T1:\n    #0 claudecord::hub::core::HubCore::tick src/hub/core.rs:1\n"
        assert analyse(noise)[1] == [] and len(analyse(noise)[0]) == 2
        assert len(analyse(ours)[1]) == 1
        assert analyse("test result: FAILED. 1 failed")[2] and not analyse("test result: ok. 3 passed\nerror: test failed, to rerun pass")[2]
        print("tsan_filter self-check passed")
        return 0
    tops, mine, failed = analyse(open(sys.argv[1], errors="replace").read())
    print(f"{len(tops)} racing accesses reported; {len(mine)} in claudeCord's own code; tests failed: {failed}")
    for f in sorted(set(mine))[:10]:
        print("  ours:", f[:160])
    return 1 if mine or failed else 0


if __name__ == "__main__":
    sys.exit(main())
