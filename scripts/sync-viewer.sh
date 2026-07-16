#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
mkdir -p docs/v/vendor
cp static/treemap.js docs/v/treemap.js
cp static/vendor/echarts.min.js docs/v/vendor/echarts.min.js
# static/vendor/echarts.min.js.sha256 names the file as "static/vendor/...", which
# only checks out relative to the repo root. Regenerate the docs/v copy's
# sidecar in place so `shasum -c` also works relative to docs/v/vendor.
(cd docs/v/vendor && shasum -a 256 echarts.min.js > echarts.min.js.sha256)
echo "docs/v/ synced"
