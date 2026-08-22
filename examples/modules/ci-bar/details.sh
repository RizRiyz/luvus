#!/bin/sh
set -eu

luvus="${LUVUS_BIN_PATH:-luvus}"
branch="${LUVUS_MODULE_BAR_VALUE:-unknown}"
"$luvus" ui notification push \
  --text "CI is passing on $branch" \
  --level success \
  --ttl-ms 4000 \
  --dedupe-key ci-details
