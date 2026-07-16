#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
mkdir -p docs/v/vendor
cp static/treemap.js docs/v/treemap.js
cp static/vendor/echarts.min.js docs/v/vendor/echarts.min.js
cp static/vendor/echarts.min.js.sha256 docs/v/vendor/echarts.min.js.sha256
echo "docs/v/ synced"
