#!/usr/bin/env bash
#
# Check a built .app: every library reference resolves to a file inside it, and everything that
# can be run without opening a window, runs.
#
#   scripts/verify_bundle.sh "path/to/Cairn.app"
#
# The load commands are rewritten before the bundle exists, against a layout that has to be
# predicted - Tauri's resource map is rooted at Contents/Resources, so `frameworks/ ->
# Frameworks/` lands one level deeper than it reads. That prediction has been wrong twice, and
# both times the pre-bundle checks passed while nothing could start. This one resolves the
# references against the bundle that was actually produced.
#
# The app binary is deliberately *not* launched: it is a GUI, and starting it to see whether it
# starts opens a window on whoever is running this.
set -uo pipefail

APP="${1:?usage: verify_bundle.sh <bundle.app>}"
failed=0

note() { printf '  %s\n' "$*"; }
bad() { printf '  FAIL %s\n' "$*" >&2; failed=1; }

rpaths_of() {
  otool -l "$1" 2>/dev/null | awk '/LC_RPATH/{f=1} f&&/ path /{print $2; f=0}'
}

# Follow every dependency of a Mach-O file to a real path.
#
# @executable_path is relative to the *executable that will load it*, which for a library is
# whichever binary pulls it in - so it is passed in rather than guessed.
resolve_all() {
  local what="$1" file="$2" exe_dir="$3"
  local dir dep candidate name found
  dir="$(cd "$(dirname "$file")" && pwd)"
  while IFS= read -r dep; do
    [ -z "$dep" ] && continue
    case "$dep" in
      /usr/lib/*|/System/*) continue ;;
      /*)
        bad "$what -> $dep (absolute path outside the bundle)"
        continue
        ;;
      @loader_path/*) candidate="${dir}/${dep#@loader_path/}" ;;
      @executable_path/*) candidate="${exe_dir}/${dep#@executable_path/}" ;;
      @rpath/*)
        name="${dep#@rpath/}"
        found=""
        while IFS= read -r rpath; do
          case "$rpath" in
            @loader_path*) candidate="${dir}${rpath#@loader_path}/$name" ;;
            @executable_path*) candidate="${exe_dir}${rpath#@executable_path}/$name" ;;
            *) candidate="$rpath/$name" ;;
          esac
          [ -f "$candidate" ] && { found=1; break; }
        done < <(rpaths_of "$file")
        [ -z "$found" ] && bad "$what -> $dep (no run-path resolves it)"
        continue
        ;;
      *) candidate="$dep" ;;
    esac
    [ -f "$candidate" ] || bad "$what -> $dep (nothing at $candidate)"
  done < <(otool -L "$file" 2>/dev/null | tail -n +2 | awk '{print $1}')
}

runs() {
  local what="$1" bin="$2"
  shift 2
  local out
  out="$("$bin" "$@" 2>&1)"
  if printf '%s' "$out" | grep -q "Library not loaded\|image not found"; then
    bad "$what does not start"
    printf '%s\n' "$out" | head -3 | sed 's/^/      /' >&2
  else
    note "$what runs"
  fi
}

echo "verifying $APP"
MACOS="$APP/Contents/MacOS"
RES="$APP/Contents/Resources/resources"
FRAMEWORKS="$APP/Contents/Resources/Frameworks"

# --- the app binary and the libraries it loads ------------------------------------------------
if [ -x "$MACOS/cairn" ]; then
  resolve_all "app binary" "$MACOS/cairn" "$MACOS"
  note "app binary: $(du -h "$MACOS/cairn" | cut -f1), references resolved"
else
  bad "no app binary in $MACOS"
fi

if [ -d "$FRAMEWORKS" ]; then
  count=0
  for lib in "$FRAMEWORKS"/*.dylib; do
    [ -f "$lib" ] || continue
    # loaded by the app binary, so @executable_path means Contents/MacOS
    resolve_all "$(basename "$lib")" "$lib" "$MACOS"
    count=$((count + 1))
  done
  note "$count libraries in Frameworks, $(du -sh "$FRAMEWORKS" | cut -f1)"
else
  note "no Frameworks directory - nothing was bundled for it"
fi

# --- the tools, which can be run without a window ---------------------------------------------
if [ -x "$RES/cairn" ]; then
  resolve_all "cairn" "$RES/cairn" "$RES"
  runs "cairn CLI" "$RES/cairn" --version
else
  bad "the CLI is not in the bundle"
fi

for tool in valhalla_build_tiles valhalla_build_admins; do
  if [ -x "$RES/valhalla/$tool" ]; then
    resolve_all "$tool" "$RES/valhalla/$tool" "$RES/valhalla"
    runs "$tool" "$RES/valhalla/$tool" --help
    outside="$(DYLD_PRINT_LIBRARIES=1 "$RES/valhalla/$tool" --help 2>&1 |
      grep -c "/opt/homebrew\|/usr/local/Cellar" || true)"
    [ "$outside" = "0" ] && note "$tool loads nothing from outside" \
      || bad "$tool loads $outside libraries from outside the bundle"
  else
    note "$tool not bundled - that step will look on PATH"
  fi
done

# --- the jar, which is optional ---------------------------------------------------------------
jar="$(ls -1 "$RES"/*-with-deps.jar 2>/dev/null | head -1 || true)"
if [ -n "$jar" ]; then
  if command -v java >/dev/null && java -jar "$jar" --help >/dev/null 2>&1; then
    note "planetiler jar runs ($(du -h "$jar" | cut -f1))"
  elif command -v java >/dev/null; then
    bad "the planetiler jar does not run - stripping may have removed something it needs"
  else
    note "planetiler jar present ($(du -h "$jar" | cut -f1)); no java here to try it"
  fi
else
  note "no jar bundled - the app fetches one on first use"
fi

[ -f "$RES/valhalla.json" ] && note "valhalla.json present" || bad "valhalla.json missing"

echo "  bundle is $(du -sh "$APP" | cut -f1)"
if [ "$failed" != "0" ]; then
  echo "bundle is not self-contained" >&2
  exit 1
fi
echo "ok: every reference resolves inside the bundle"
