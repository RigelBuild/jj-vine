#!/usr/bin/env bash
# Check version selection, deterministic mint targets, and collisions.
set -u
script="$(cd "$(dirname "$0")" && pwd)/release-tag.sh"
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT
fails=0
mkdir -p "$dir/bin" "$dir/repo"
cat >"$dir/bin/gh" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *"--method POST"*)
    printf '%s\n' "$*" >>"$GH_LOG"
    if [ "${GH_MODE:-create}" = collision ]; then
      : >"$GH_MARKER"
      echo 'Reference already exists' >&2
      echo 'HTTP 422' >&2
      exit 1
    fi
    ;;
  *"git/ref/tags/"*)
    if [ "${GH_MODE:-create}" = existing ]; then exit 0; fi
    if [ "${GH_MODE:-create}" = collision ] && [ -e "$GH_MARKER" ]; then
      printf '%s\n' "$GH_EXISTING_SHA"
      exit 0
    fi
    exit 1
    ;;
  *) echo "unexpected gh call: $*" >&2; exit 2 ;;
esac
EOF
chmod +x "$dir/bin/gh"
export PATH="$dir/bin:$PATH"
export GH_LOG="$dir/gh.log"
export GH_MARKER="$dir/gh-marker"
cd "$dir/repo" || exit 1
git init -q -b rigel-release
git config user.name test
git config user.email test@example.invalid
# Dependency tables carry their own `version` keys; only `[package]` counts.
write_version() {
  printf '[package]\nname = "jj-vine"\nversion = "%s"\n\n[dependencies.serde]\nversion = "1.0.0"\n' "$1" >Cargo.toml
  git add Cargo.toml
  git commit -q -m "$1"
  git rev-parse HEAD
}
old="$(write_version 0.5.4-rigel.1)"
base="$old"
new="$(write_version 0.5.4-rigel.2)"
git tag v0.5.4-rigel.1 "$old"

expect_rc() {
  local name="$1" want="$2"; shift 2
  local rc
  "$@" >/dev/null 2>&1
  rc=$?
  if [ "$rc" -ne "$want" ]; then
    echo "FAIL ${name}: rc=${rc} (want ${want})"
    fails=$((fails + 1))
  else
    echo "ok   ${name}"
  fi
}
expect_rc 'next version passes PR check' 0 "$script" check-pr "$base" "$new"
skip="$(write_version 0.5.4-rigel.4)"
expect_rc 'skipped suffix fails PR check' 1 "$script" check-pr "$new" "$skip"
git checkout -q -b dep-bump "$new"
sed -i 's/^version = "1.0.0"$/version = "1.0.1"/' Cargo.toml
git commit -q -am dep-bump
noop="$(git rev-parse HEAD)"
expect_rc 'dependency-only Cargo.toml change passes PR check' 0 "$script" check-pr "$new" "$noop"

GH_MODE=create "$script" mint "$noop" RigelBuild/jj-vine >/dev/null 2>&1
if [ "$?" -eq 0 ] && grep -q 'refs/tags/v0.5.4-rigel.2' "$GH_LOG"; then
  echo 'ok   mint creates the package version tag'
else
  echo 'FAIL mint creates the package version tag'
  fails=$((fails + 1))
fi
if grep -q "sha=${new}" "$GH_LOG"; then
  echo 'ok   mint targets the commit that set the version'
else
  echo 'FAIL mint targets the commit that set the version'
  fails=$((fails + 1))
fi
before="$(wc -l <"$GH_LOG")"
GH_MODE=existing "$script" mint "$noop" RigelBuild/jj-vine >/dev/null 2>&1
if [ "$?" -eq 0 ] && [ "$(wc -l <"$GH_LOG")" -eq "$before" ]; then
  echo 'ok   existing tag is a no-op'
else
  echo 'FAIL existing tag is a no-op'
  fails=$((fails + 1))
fi
GH_MODE=collision GH_EXISTING_SHA="$new" "$script" mint "$new" RigelBuild/jj-vine >/dev/null 2>&1
if [ "$?" -eq 0 ]; then echo 'ok   same-target 422 succeeds'; else echo 'FAIL same-target 422 succeeds'; fails=$((fails + 1)); fi
rm -f "$GH_MARKER"
GH_MODE=collision GH_EXISTING_SHA="$old" "$script" mint "$new" RigelBuild/jj-vine >/dev/null 2>&1
if [ "$?" -ne 0 ]; then echo 'ok   conflicting 422 fails'; else echo 'FAIL conflicting 422 fails'; fails=$((fails + 1)); fi

[ "$fails" -eq 0 ]
