#!/bin/sh
# A file row was clicked: open it in a pager, in a new pane (default) or a new
# tab. This is the prototype's "view a file" -- a real PTY running a pager, which
# is the most a module can do without core support for a native view pane
# (docs/38). `bat` is used when present (syntax + line numbers), else `less -N`.
set -eu
. "$(dirname "$0")/lib.sh"

path="${LUVUS_MODULE_ROW_VALUE:-}"
[ -n "$path" ] || { "$luvus" ui toast "no file selected"; exit 1; }
[ -f "$path" ] || { "$luvus" ui toast "not a file: ${path##*/}"; exit 1; }

open_in="${LUVUS_SETTING_OPEN_IN:-pane}"

# The first pane id in a JSON reply. `pane split` returns exactly the new pane;
# after `tab new` the new tab has a single (focused) shell, so its first pane is
# the one we want. Pretty-printed JSON, so match `"pane": "N"`.
first_pane() { sed -n 's/.*"pane": *"\([0-9][0-9]*\)".*/\1/p' | head -1; }

if [ "$open_in" = "tab" ]; then
  "$luvus" tab new >/dev/null 2>&1 || { "$luvus" ui toast "could not open a tab"; exit 1; }
  pid=$("$luvus" pane list --current-tab | first_pane)
else
  pid=$("$luvus" pane split | first_pane)
fi

[ -n "${pid:-}" ] || { "$luvus" ui toast "could not open a pane"; exit 1; }

# Single-quote the path for the shell that will run the pager, escaping any
# embedded single quotes so a spaced or quirky filename is safe.
q=$(printf "%s" "$path" | sed "s/'/'\\\\''/g")
if command -v bat >/dev/null 2>&1; then
  cmd="bat --paging=always --style=numbers,header '$q'"
else
  cmd="less -N '$q'"
fi

"$luvus" pane run "$pid" "$cmd"
"$luvus" pane focus "$pid" >/dev/null 2>&1 || true
"$luvus" ui toast "opened ${path##*/}"
