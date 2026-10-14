#!/usr/bin/env bash
#
# Check a built .deb: every library reference resolves inside it, and the tools that can run
# without a window, run.
#
#   scripts/verify_deb.sh path/to/cairn_0.1.0_amd64.deb
#
# The Linux counterpart of verify_bundle.sh, and it exists for the same reason: the rpaths are
# written before the package exists, against a layout that has to be predicted. On macOS that
# prediction was wrong twice while every pre-bundle check passed.
#
# Extraction is enough to check it - $ORIGIN is resolved against the file being loaded, so an
# unpacked tree answers the same question an installed one would, without needing root.
#
# The app binary is deliberately not launched: it opens a window.
set -uo pipefail

DEB="${1:?usage: verify_deb.sh <package.deb>}"
failed=0
note() { printf '  %s\n' "$*"; }
bad() { printf '  FAIL %s\n' "$*" >&2; failed=1; }

root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT
dpkg-deb -x "$DEB" "$root"

echo "verifying $(basename "$DEB")"

binary="$(ls -1 "$root"/usr/bin/* 2>/dev/null | head -1)"
[ -n "$binary" ] || { echo "  FAIL no binary in usr/bin" >&2; exit 1; }
libdir="$(ls -d "$root"/usr/lib/*/Frameworks 2>/dev/null | head -1)"
resources="$(ls -d "$root"/usr/lib/*/resources 2>/dev/null | head -1)"

# Every dependency has to resolve to a file. ldd reports what the loader would do, $ORIGIN and
# all, so this is the real answer for this layout rather than a reading of the rpath.
resolves() {
  local what="$1" file="$2" out
  out="$(env -u LD_LIBRARY_PATH ldd "$file" 2>/dev/null || true)"
  local missing
  missing="$(printf '%s' "$out" | awk '/not found/{print $1}' | tr '\n' ' ')"
  if [ -n "$missing" ]; then
    bad "$what cannot resolve: $missing"
  else
    note "$what: $(printf '%s' "$out" | grep -c '=>') dependencies resolve"
  fi
}

resolves "app binary" "$binary"

if [ -n "$libdir" ]; then
  carried="$(ls -1 "$libdir" | wc -l | tr -d ' ')"
  note "$carried libraries carried, $(du -sh "$libdir" | cut -f1)"
  # Carried and yet loaded from the system means the rpath did not take - which looks fine on the
  # build machine and fails on a release with different sonames, the exact thing being prevented.
  # ldd prints the path it walked, not a tidy one - an rpath of $ORIGIN/../lib/... comes back as
  # usr/bin/../lib/..., which no string comparison against the directory will match. Normalise
  # both sides before comparing.
  libdir_real="$(cd "$libdir" && pwd -P)"
  deps="$(env -u LD_LIBRARY_PATH ldd "$binary" "$libdir"/* 2>/dev/null | awk '/=>/ && $3 != "" {print $1, $3}' |
    while read -r soname path; do
      printf '%s %s\n' "$soname" "$(cd "$(dirname "$path")" 2>/dev/null && pwd -P)"
    done | sort -u)"
  for lib in "$libdir"/*; do
    [ -f "$lib" ] || continue
    soname="$(basename "$lib")"
    case "$deps" in
      *"$soname $libdir_real"*) ;;
      *) bad "$soname is carried but the system copy is what loads" ;;
    esac
  done
else
  bad "no Frameworks directory - nothing was carried, and the sonames differ between releases"
fi

# libstdc++ is static in these builds. ldd walks the whole closure and the carried libraries
# bring their own, so only the direct NEEDED entries can answer this.
if command -v patchelf >/dev/null; then
  case "$(patchelf --print-needed "$binary" 2>/dev/null || true)" in
    *libstdc++*) bad "the app links libstdc++ dynamically; it will not start on an older release" ;;
    *) note "libstdc++ is static in the app binary" ;;
  esac
fi

if [ -n "$resources" ] && [ -x "$resources/cairn" ]; then
  resolves "cairn CLI" "$resources/cairn"
  out="$(env -u LD_LIBRARY_PATH "$resources/cairn" --version 2>&1)"
  case "$out" in
    *"not found"*|*"cannot open shared object"*) bad "the CLI does not start: $out" ;;
    *) note "cairn CLI runs: $out" ;;
  esac
else
  bad "the CLI is not in the package"
fi

for tool in valhalla_build_tiles valhalla_build_admins; do
  if [ -n "$resources" ] && [ -x "$resources/valhalla/$tool" ]; then
    resolves "$tool" "$resources/valhalla/$tool"
  else
    note "$tool not packaged - that step will look on PATH"
  fi
done

[ -n "$resources" ] && [ -f "$resources/valhalla.json" ] && note "valhalla.json present" ||
  bad "valhalla.json missing"

note "package is $(du -sh "$root" | cut -f1) installed"
if [ "$failed" != "0" ]; then
  echo "package is not self-contained" >&2
  exit 1
fi
echo "ok: every reference resolves inside the package"
