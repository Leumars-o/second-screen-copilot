#!/usr/bin/env bash
# Generates the SYNTHETIC TTS clips listed in bench/data/stt/manifest.jsonl with macOS `say`
# (16 kHz mono PCM16). Clips are git-ignored. Falls back to the default voice if one is missing.
set -euo pipefail
cd "$(dirname "$0")/.."
out=data/stt/clips
mkdir -p "$out"

clip() { # id voice text
  local id=$1 voice=$2 text=$3 args=()
  if [[ -n "$voice" ]] && say -v '?' | awk '{print $1}' | grep -qx "$voice"; then args=(-v "$voice"); fi
  say ${args[@]+"${args[@]}"} -o "$out/$id.wav" --file-format=WAVE --data-format=LEI16@16000 "$text"
  echo "$out/$id.wav"
}

clip tts-us-pricing "" "What would forty seats cost us per year, and is single sign-on included?"
clip tts-us-onboarding "" "How long does onboarding usually take for a team of our size?"
clip tts-gb-security Daniel "Is our data encrypted at rest, and can it stay in the European Union?"
clip tts-in-discount Aman "Do you offer a discount for non-profit organisations?"
