#!/bin/sh
# Fake `gh` for tests. `gh gist create --secret <file> -d <desc>` -> prints a
# canned gist URL and records the snapshot file it was handed for inspection.
# `DUH_STUB_FAIL=1` makes it simulate a real `gh` failure (nonzero exit,
# message on stderr) so the 502 error-mapping path can be exercised.
# Any other invocation exits 1.
case "$1 $2" in
  "gist create")
    if [ -n "$DUH_STUB_FAIL" ]; then
      echo "gh: could not create gist: HTTP 401: Bad credentials" >&2
      exit 1
    fi
    # last arg that exists as a file is the snapshot; copy it for the test to read
    for a in "$@"; do [ -f "$a" ] && cp "$a" "${DUH_STUB_CAPTURE:-/tmp/duh-gist-capture.txt}"; done
    echo "https://gist.github.com/tester/deadbeefcafebabefeed0000deadbeef"
    exit 0 ;;
  *) echo "unexpected gh invocation: $*" >&2; exit 1 ;;
esac
