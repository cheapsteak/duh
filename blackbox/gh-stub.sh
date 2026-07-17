#!/bin/sh
# Fake `gh` for tests. `gh gist create <file> -d <desc>` -> prints a canned
# gist URL and records the snapshot file it was handed for inspection.
# (Gists are secret by default; `gh` has no `--secret` flag.)
#
# Enforces secret-only as a regression test: if ANY arg is `-p`/`--public`,
# this stub refuses and exits 2 instead of silently going along with it — so
# if `create_gist` in src/serve.rs is ever changed to pass `--public`, the
# stub fails and the share tests break.
#
# `DUH_STUB_FAIL=1` makes it simulate a real `gh` failure (nonzero exit,
# message on stderr) so the 502 error-mapping path can be exercised.
# `DUH_STUB_CAPTURE` must be set to the path to copy the snapshot file to —
# no shared `/tmp` fallback, so a test that forgets to set it fails loudly
# instead of silently reading/writing a world-shared file.
# Any other invocation exits 1.
case "$1 $2" in
  "gist create")
    for a in "$@"; do
      case "$a" in
        -p|--public) echo "REFUSING --public" >&2; exit 2 ;;
      esac
    done
    if [ -z "${DUH_STUB_CAPTURE:-}" ]; then
      echo "DUH_STUB_CAPTURE must be set" >&2
      exit 3
    fi
    if [ -n "$DUH_STUB_FAIL" ]; then
      echo "gh: could not create gist: HTTP 401: Bad credentials" >&2
      exit 1
    fi
    # last arg that exists as a file is the snapshot; copy it for the test to read
    for a in "$@"; do [ -f "$a" ] && cp "$a" "$DUH_STUB_CAPTURE"; done
    echo "https://gist.github.com/tester/deadbeefcafebabefeed0000deadbeef"
    exit 0 ;;
  *) echo "unexpected gh invocation: $*" >&2; exit 1 ;;
esac
