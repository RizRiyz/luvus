#!/bin/sh
# Publish one bounded, structured widget. In a real module, replace these values
# from a CI webhook, event hook, or cached status command. No work runs in the
# Luvus render loop.
set -eu

luvus="${LUVUS_BIN_PATH:-luvus}"

"$luvus" bar push \
  --id status \
  --region top-right \
  --priority 60 \
  --content '[{"type":"text","text":"CI","tone":"muted"},{"type":"separator"},{"type":"state","state":"done","label":"passing","tone":"success"},{"type":"badge","text":"12","tone":"accent","action":"details","value":"main"}]' \
  --compact-content '[{"type":"text","text":"CI"},{"type":"state","state":"done","tone":"success","action":"details","value":"main"}]'
