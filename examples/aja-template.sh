#!/bin/sh
# Minimal AJA template: reads one Assignment JSON, returns one Report JSON.
# Replace the body with a real worker. It reports "blocked" on purpose so it
# never claims work it did not do.
assignment=$(cat)
printf '%s' '{"status":"blocked","summary":"template only; replace this AJA","artifacts":[],"evidence":[],"open_questions":["wire a real worker"]}'
