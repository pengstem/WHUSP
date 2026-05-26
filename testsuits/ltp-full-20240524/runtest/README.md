# LTP Runtest Manifests

This directory vendors the full LTP 20240524 `runtest` manifest set from the
contest testsuite checkout. `scripts/export_contest_case_scripts.py` uses these
files to turn `os/src/task/ltp_whitelist.txt` case names into guest-side
commands.

It is not a full LTP checkout and does not contain testcase binaries. The
generated `disk.img` still runs binaries from the official mounted test disk;
these files only make contest script disk generation independent from a sibling
`../testsuits-for-oskernel` checkout or any evaluator-local source layout.
