#!/usr/bin/env bash
# Static acceptance checks for release-tag Requirement 1.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
release="$root/.github/workflows/release-tag.yaml"
fail() { echo "FAIL $1" >&2; exit 1; }
pass() { echo "ok   $1"; }
expect() {
  local name="$1" actual="$2" expected="$3"
  [ "$actual" = "$expected" ] || fail "$name (got '$actual', expected '$expected')"
}

[ -f "$release" ] || fail 'release-tag.yaml exists'
if grep -Eq '^[[:space:]]*on:' "$release"; then fail 'workflow on key is quoted'; fi
expect 'release pull requests target rigel-release' "$(yq -r '.on.pull_request.branches[0]' "$release")" rigel-release
expect 'release pushes target rigel-release' "$(yq -r '.on.push.branches[0]' "$release")" rigel-release
expect 'mint workflow supports dispatch' "$(yq -r '.on | has("workflow_dispatch")' "$release")" true
expect 'dispatch takes no inputs' "$(yq -r '.on.workflow_dispatch.inputs // "none"' "$release")" none
expect 'check job is pull-request only' "$(yq -r '.jobs.check.if' "$release")" "github.event_name == 'pull_request'"
expect 'mint excludes pull requests' "$(yq -r '.jobs.mint.if' "$release")" "github.event_name != 'pull_request'"
expect 'check has contents read only' "$(yq -r '.jobs.check.permissions | keys | join(",")' "$release")" contents
expect 'workflow token has contents read only' "$(yq -r '.permissions | keys | join(",")' "$release")" contents
expect 'check validates changed version source' "$(yq -r '.jobs.check.steps | map(select(.name == "Validate a changed release version")) | length' "$release")" 1
expect 'mint uses release environment' "$(yq -r '.jobs.mint.environment' "$release")" rigel-release
expect 'dispatch guard is the first mint step' "$(yq -r '.jobs.mint.steps[0].name' "$release")" 'Require release-branch dispatch'
expect 'mint checks out full history' "$(yq -r '.jobs.mint.steps | map(select(.name == "Checkout")) | .[0].with.fetch-depth' "$release")" 0
token='.jobs.mint.steps | map(select(.uses // "" | test("actions/create-github-app-token"))) | .[0]'
expect 'mint takes an App token on every run' "$(yq -r ".jobs.mint.steps | map(select(.uses // \"\" | test(\"actions/create-github-app-token\"))) | length" "$release")" 1
expect 'mint token step has no condition' "$(yq -r "$token | .if // \"none\"" "$release")" none
expect 'mint uses the E1 client ID' "$(yq -r "$token | .with.client-id" "$release")" '${{ vars.AUTOMATION_APP_CLIENT_ID }}'
expect 'mint uses the E1 private key' "$(yq -r "$token | .with.private-key" "$release")" '${{ secrets.AUTOMATION_APP_PRIVATE_KEY }}'
expect 'mint token has contents write only' "$(yq -r "$token | .with | keys | map(select(test(\"^permission-\"))) | join(\",\")" "$release")" permission-contents
expect 'mint token has contents write' "$(yq -r "$token | .with.permission-contents" "$release")" write
expect 'mint token is repository scoped' "$(yq -r "$token | .with.repositories" "$release")" jj-vine
expect 'mint has no concurrency' "$(yq -r '.jobs.mint.concurrency // "none"' "$release")" none
# A job that runs a repo script must check out before it; a bare runner has no tree.
for job in $(yq -r '.jobs | keys | .[]' "$release"); do
  first_script="$(yq -r ".jobs.\"$job\".steps | to_entries | map(select(.value.run // \"\" | test(\"\\.github/scripts/\"))) | .[0].key // \"\"" "$release")"
  [ -n "$first_script" ] || continue
  first_checkout="$(yq -r ".jobs.\"$job\".steps | to_entries | map(select(.value.uses // \"\" | test(\"^actions/checkout@\"))) | .[0].key // \"\"" "$release")"
  [ -n "$first_checkout" ] && [ "$first_checkout" -lt "$first_script" ] || fail "$job checks out before running repo scripts"
  pass "$job checks out before running repo scripts"
done
while IFS= read -r line; do
  [[ "$line" =~ uses:[[:space:]]+[^[:space:]#]+@[0-9a-f]{40}[[:space:]]+#\ v[^[:space:]]+[[:space:]]*$ ]] || fail "action is not SHA-pinned with a version trailer: $line"
done < <(grep -h 'uses:' "$release")
pass 'release-tagging Requirement 1 static contract and SHA-pinned actions'
