#!/bin/sh
# Copyright © 2026 Jalapeno Labs
#
# A stand-in harness for smoke testing a built image.
#
# The satellite spawns this exactly as it would spawn a real CLI and cannot tell
# the difference, so the shipped image is exercised rather than a special build
# of it. Mount this and a recorded transcript into the container and point
# ARSOX_CLAUDE_BIN here.
#
# Behaviour rides on the prompt as [[key=value]] directives, for the same reason
# it does in the Rust stand-in: the prompt belongs to one spawn, and process
# environment is shared by every spawn. The two stand-ins share the vocabulary
# they both implement, so a smoke check written against one runs against either:
#
# - [[exit=N]] exits with N rather than 0, for the crash path.
# - [[unrecognized=N]] emits N lines of an event type nothing maps, before the
#   transcript, for the degraded incident a later CLI's new event type produces.
set -eu

TRANSCRIPT="${ARSOX_FAKE_TRANSCRIPT:-/fixtures/tool-call.stdout.jsonl}"

# The satellite passes the prompt as the argument after --print.
prompt=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--print" ] && [ "$#" -gt 1 ]; then
    prompt="$2"
    break
  fi
  shift
done

case "$prompt" in
  *"[[exit="*)
    code="${prompt#*[[exit=}"
    code="${code%%]]*}"
    ;;
  *)
    code=0
    ;;
esac

case "$prompt" in
  *"[[unrecognized="*)
    unrecognized="${prompt#*[[unrecognized=}"
    unrecognized="${unrecognized%%]]*}"
    ;;
  *)
    unrecognized=0
    ;;
esac

# An event type nothing maps, which is what a CLI release adding one looks like
# from here. The mapper records a degraded incident and drops the line, so the
# turn still finishes and the smoke test has an incident to read back.
emitted=0
while [ "$emitted" -lt "$unrecognized" ]; do
  printf '{"type":"an_event_type_from_a_later_cli"}\n'
  emitted=$((emitted + 1))
done

# Line at a time, so the satellite reads this as the stream it is rather than one
# buffered blob.
while IFS= read -r line; do
  [ -z "$line" ] && continue
  printf '%s\n' "$line"
done < "$TRANSCRIPT"

exit "$code"
