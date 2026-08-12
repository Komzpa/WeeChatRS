#!/usr/bin/env bash
set -euo pipefail

target_dir="${CARGO_TARGET_DIR:-/tmp/weechatrs-changed-surfaces-target}"
export CARGO_TARGET_DIR="$target_dir"

filters=(
  harness
  reply
  history
  upgrade
  preview
)

for filter in "${filters[@]}"; do
  cargo test --quiet "$filter"
done
