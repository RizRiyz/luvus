#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 4 ]; then
    echo "usage: $0 <workspace-id> [IANA-timezone] [session] [idempotency-key]" >&2
    exit 2
fi

workspace_id=$1
timezone=${2:-UTC}
session=${3:-}
idempotency_key=${4:-weekday-readonly-review-$workspace_id}

if [ -n "$session" ]; then
    set -- --session "$session"
else
    set --
fi

luvus "$@" automation preview \
    --weekly mon,tue,wed,thu,fri \
    --at 08:00 \
    --timezone "$timezone"

luvus "$@" automation create "Weekday read-only review" \
    --title "Review workspace changes" \
    --prompt "Inspect the current changes. Report correctness, security, and test risks without modifying files." \
    --agent codex \
    --workspace-id "$workspace_id" \
    --weekly mon,tue,wed,thu,fri \
    --at 08:00 \
    --timezone "$timezone" \
    --mode workspace \
    --access read-only \
    --misfire skip \
    --misfire-grace 3600 \
    --overlap skip \
    --idempotency-key "$idempotency_key"
