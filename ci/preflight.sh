#!/usr/bin/env bash
# Which self-hosted runner families are available for this run?
#
# WHY THIS IS A DECLARATION AND NOT AN API CALL — do not "fix" this into `gh api`:
# listing self-hosted runners requires the `administration` permission, which is NOT among the
# permissions a workflow GITHUB_TOKEN can be granted. `gh api repos/:owner/:repo/actions/runners`
# 403s from inside Actions. It works from a developer shell only because a personal token carries
# admin scope — and ci/runners/README.md documents it that way, which is the trap. So the fleet is
# DECLARED in the repo variable CI_RUNNERS and read here.
#
# Usage:
#   ci/preflight.sh              # `family=true|false` lines, one per family, for $GITHUB_OUTPUT
#   ci/preflight.sh --summary    # a markdown report, for $GITHUB_STEP_SUMMARY
#
# THE DEFAULT IS DELIBERATELY CONSERVATIVE. An unset or blank CI_RUNNERS means "macos only", never
# "everything". A default of everything-available reproduces the exact bug this script exists to
# prevent: jobs scheduled against runners that do not exist, queueing for 24h before GitHub cancels
# them, at 0 elapsed, with no red signal anywhere. Five weeks of dark CI came from that.
#
# The summary deliberately does NOT enumerate the skipped JOBS. GitHub already lists them by name in
# the run, and a hand-maintained job list here would be a fourth thing to drift. What the summary
# owes the reader is the REASON and the declared value — so a stale declaration is visible at the
# point where it does damage.
set -euo pipefail

FAMILIES="macos linux windows"
DEFAULT="macos"

declared="${CI_RUNNERS:-}"
if [ -z "${declared//[[:space:]]/}" ]; then
  declared="$DEFAULT"
fi

# Lowercase, strip all whitespace, and wrap in commas on BOTH ends so a lookup matches a whole
# family name. Without the wrapping, a declared "linux-arm64" would satisfy a "linux" lookup and
# schedule x86 FUSE jobs onto an arm box.
normalised=",$(printf '%s' "$declared" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]'),"

available() {
  case "$normalised" in
    *",$1,"*) return 0 ;;
    *)        return 1 ;;
  esac
}

if [ "${1:-}" = "--summary" ]; then
  absent=""
  for family in $FAMILIES; do
    available "$family" || absent="$absent \`$family\`"
  done

  echo "## CI runner preflight"
  echo
  echo "Declared fleet (repo variable \`CI_RUNNERS\`): \`$declared\`"
  echo
  echo "| Runner family | Status |"
  echo "|---|---|"
  for family in $FAMILIES; do
    if available "$family"; then
      echo "| \`$family\` | ✅ available |"
    else
      echo "| \`$family\` | ⛔ **absent — its jobs were SKIPPED** |"
    fi
  done
  echo

  if [ -n "$absent" ]; then
    echo "> **This coverage was skipped, not passed.** Jobs requiring$absent were never scheduled,"
    echo "> so a green run here is green about less than a full run would be. The skipped jobs are"
    echo "> listed by name in this run's job list."
    echo ">"
    echo "> Register the runner (\`ci/runners/README.md\`) and add the family to the \`CI_RUNNERS\`"
    echo "> repo variable, or accept the gap knowingly."
  else
    echo "> Full fleet declared; no coverage skipped."
  fi
  exit 0
fi

for family in $FAMILIES; do
  if available "$family"; then
    echo "$family=true"
  else
    echo "$family=false"
  fi
done
