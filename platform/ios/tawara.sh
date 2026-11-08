#!/usr/bin/env bash
# Copyright (c) 2026 Adequate Systems HK Limited
# Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.
# Builds and checks Tawara for iOS on a macOS runner:
#
#   platform/ios/tawara.sh device      # build and link for aarch64-apple-ios;
#                                      # installing on a phone needs signing
#   platform/ios/tawara.sh simulator   # build, bundle, sign ad hoc, boot a
#                                      # simulator, install, run the checks
#
# There is no Xcode project: the bundle is the crates/mobile `tawara` binary
# and an Info.plist generated from Info.plist.in. Device and store signing is
# done by platform/ios/release.sh.
#
# The application's stderr goes to the file given to `simctl launch --stderr`
# and contains `TAWARA ...` lines, never a secret. This script prints
# `CHECK PASS|FAIL <name>: <detail>` lines and a table in the step summary, and
# fails if any check failed.
set -Eeuo pipefail

BIN=tawara
BUNDLE=com.adequatelimited.tawara
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# bn_number gives the build number used as CFBundleVersion.
# shellcheck source=../build-number.sh
. "$ROOT/platform/build-number.sh"
OUT=${RUNNER_TEMP:-/tmp}/tawara-ios
export IPHONEOS_DEPLOYMENT_TARGET=${IPHONEOS_DEPLOYMENT_TARGET:-16.0}
mkdir -p "$OUT/shots"
: >"$OUT/results.tsv"
# Truncate the stderr log so that lines of an earlier run are not checked.
: >"$OUT/app.err"

group() { echo "::group::$*"; }
endgroup() { echo "::endgroup::"; }
die() { echo "::error::$*"; exit 1; }
record() { # record NAME PASS|FAIL DETAIL
  printf '%s\t%s\t%s\n' "$1" "$2" "${3:-}" >>"$OUT/results.tsv"
  echo "CHECK $2 $1: ${3:-}"
}
check() { # check NAME DETAIL COMMAND...: PASS when the command succeeds
  local name=$1 detail=$2
  shift 2
  if "$@"; then record "$name" PASS "$detail"; else record "$name" FAIL "$detail"; fi
}
platform_of() { otool -l "$1" | awk '/LC_BUILD_VERSION/{f=1} f&&/platform/{print $2; exit}'; }
version() {
  cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="tawara-mobile"))'
}

# -------------------------------------------------------------------- device
device() {
  xcodebuild -version
  cd "$ROOT"
  group "cargo build --target aarch64-apple-ios"
  cargo build --locked --release -p tawara-mobile --bin "$BIN" --target aarch64-apple-ios
  endgroup
  local exe=target/aarch64-apple-ios/release/$BIN
  file "$exe"
  # LC_BUILD_VERSION platform 2 is iOS on a device.
  check device.build "$(file -b "$exe")" test "$(platform_of "$exe")" = 2
  summary
  ! grep -q $'\tFAIL\t' "$OUT/results.tsv"
}

# ----------------------------------------------------------------- simulator
wait_after() { # wait_after FILE LINE ERE SECONDS: first match after line LINE, or fail on timeout
  local t=0 line
  while :; do
    line=$(tail -n +"$(($2 + 1))" "$1" 2>/dev/null | grep -E -m1 "$3" || true)
    [ -n "$line" ] && { echo "$line"; return 0; }
    [ "$t" -ge "$4" ] && return 1
    sleep 1
    t=$((t + 1))
  done
}
# shot NAME: takes a simulator screenshot. Any earlier file of the same name is
# removed first, so a failed screenshot is missing rather than stale.
shot() {
  rm -f "$OUT/shots/$1.png"
  xcrun simctl io "$UDID" screenshot --type=png "$OUT/shots/$1.png" >/dev/null 2>&1 || true
}
# light_pixels PNG: counts pixels in the top 6% of PNG (the status bar band)
# with luminance above 160 of 255. sips converts to BMP, which plain python3
# parses (24 or 32 bits, either row order). sips exits 0 even when it cannot
# read the input, so a stale BMP is removed first.
light_pixels() {
  rm -f "$OUT/light.bmp"
  sips -s format bmp "$1" --out "$OUT/light.bmp" >/dev/null 2>&1 || return 1
  [ -s "$OUT/light.bmp" ] || return 1
  python3 - "$OUT/light.bmp" <<'PY'
import struct, sys
data = open(sys.argv[1], "rb").read()
off = struct.unpack_from("<I", data, 10)[0]
w, h = struct.unpack_from("<ii", data, 18)
bpp = struct.unpack_from("<H", data, 28)[0] // 8
top_down = h < 0
h = abs(h)
stride = (w * bpp * 8 + 31) // 32 * 4
band = max(1, round(h * 0.06))
n = 0
for y in range(band):
    row = off + (y if top_down else h - 1 - y) * stride
    for x in range(w):
        p = row + x * bpp
        if 0.299 * data[p + 2] + 0.587 * data[p + 1] + 0.114 * data[p] > 160:
            n += 1
print(n)
PY
}
# wait_pasteboard UDID: prints the simulator pasteboard text, polling every
# 0.25 s for about five seconds until it is non-empty, because `simctl pbpaste`
# can lag behind the copy. The copy lasts two minutes (PASTEBOARD_LIFETIME), so
# polling cannot outlast it. Returns the status of the last read, so a failing
# read is reported as such and not as an empty pasteboard.
wait_pasteboard() {
  local i=0 text status
  while :; do
    status=0
    text=$(xcrun simctl pbpaste "$1" 2>/dev/null) || status=$?
    if [ "$status" -eq 0 ] && [ -n "$text" ]; then break; fi
    [ "$i" -ge 20 ] && break
    sleep 0.25
    i=$((i + 1))
  done
  printf '%s' "$text"
  return "$status"
}

simulator() {
  xcodebuild -version
  cd "$ROOT"
  group "cargo build --target aarch64-apple-ios-sim"
  cargo build --locked --release -p tawara-mobile --bin "$BIN" --target aarch64-apple-ios-sim
  endgroup
  local exe=target/aarch64-apple-ios-sim/release/$BIN
  # LC_BUILD_VERSION platform 7 is the iOS simulator.
  [ "$(platform_of "$exe")" = 7 ] || die "$exe: platform $(platform_of "$exe"), expected 7"

  group "bundle and sign"
  local app=$OUT/Tawara.app build
  # The development number, or the committed counter for a store build; an
  # invalid TAWARA_BUILD_NUMBER ends the script.
  build=$(bn_number)
  rm -rf "$app"
  mkdir -p "$app"
  cp "$exe" "$app/$BIN"
  sed -e 's/@PLATFORM@/iPhoneSimulator/' -e "s/@MINOS@/$IPHONEOS_DEPLOYMENT_TARGET/" \
    -e "s/@VERSION@/$(version)/" -e "s/@BUILD@/$build/" platform/ios/Info.plist.in >"$app/Info.plist"
  plutil -lint "$app/Info.plist"
  check bundle.build "CFBundleVersion is '$build'" \
    test "$(plutil -extract CFBundleVersion raw -o - "$app/Info.plist")" = "$build"
  # Without the scene manifest, UIKit stops an app linked with the iOS 27 SDK
  # at launch; older SDKs still run it, so the simulator alone would not catch
  # its absence.
  check bundle.scene_manifest "UIApplicationSceneManifest in Info.plist" \
    plutil -extract UIApplicationSceneManifest.UISceneConfigurations xml1 -o /dev/null "$app/Info.plist"
  # The status bar must be light on the dark screens. These two keys make UIKit
  # take the style from UIStatusBarStyle instead of the iced view controller,
  # whose default leaves it to UIKit. The start screenshot is checked below.
  local style appearance
  style=$(plutil -extract UIStatusBarStyle raw -o - "$app/Info.plist" 2>/dev/null || true)
  appearance=$(plutil -extract UIViewControllerBasedStatusBarAppearance raw -o - "$app/Info.plist" 2>/dev/null || true)
  check bundle.status_bar "UIStatusBarStyle is '$style'" test "$style" = UIStatusBarStyleLightContent
  check bundle.status_bar_appearance "UIViewControllerBasedStatusBarAppearance is '$appearance'" test "$appearance" = false
  codesign --force --sign - --timestamp=none "$app"
  codesign --verify --verbose=2 "$app"
  endgroup

  group "simulator"
  local runtime devtype
  read -r runtime devtype < <(python3 - <<'PY'
import json, subprocess
rts = json.loads(subprocess.check_output(["xcrun", "simctl", "list", "-j", "runtimes", "available"]))["runtimes"]
ios = [r for r in rts if r.get("platform") == "iOS"]
rt = max(ios, key=lambda r: tuple(int(x) for x in r["version"].split(".")))
phones = [d for d in rt.get("supportedDeviceTypes", []) if d.get("productFamily") == "iPhone" or d["name"].startswith("iPhone")]
named = [d for d in phones if d["name"] == "iPhone 17"]
print(rt["identifier"], (named or phones)[-1]["identifier"])
PY
  )
  echo "runtime $runtime; device type $devtype"
  # UDID is the device this run creates, and cleanup deletes only that device.
  # It is cleared first so that no value from the environment is used. The trap
  # is set before creation so it runs whichever step fails; if creation fails,
  # UDID stays empty.
  UDID=
  trap cleanup EXIT
  UDID=$(xcrun simctl create tawara "$devtype" "$runtime")
  xcrun simctl boot "$UDID"
  xcrun simctl bootstatus "$UDID" -b
  xcrun simctl install "$UDID" "$app"
  # The checks below bring Settings to the front. Its first launch on a new
  # simulator is slow, so launch it once now and the timed waits measure the
  # app rather than the Settings cold start.
  xcrun simctl launch "$UDID" com.apple.Preferences >/dev/null
  sleep 10
  xcrun simctl terminate "$UDID" com.apple.Preferences >/dev/null 2>&1 || true
  endgroup

  local err=$OUT/app.err out pid pid2 line container
  group "start"
  out=$(SIMCTL_CHILD_RUST_BACKTRACE=1 xcrun simctl launch --stdout="$OUT/app.out" \
    --stderr="$err" "$UDID" "$BUNDLE")
  pid=$(awk '{print $NF}' <<<"$out")
  line=$(wait_after "$err" 0 '^TAWARA start: ' 60 || true)
  check start "${line:-no start line}" grep -q 'Library/Application Support/Tawara$' <<<"$line"
  line=$(wait_after "$err" 0 '^TAWARA store: ' 5 || true)
  check store.protected "${line:-no store line}" grep -q 'protected=true' <<<"$line"
  check store.excluded "${line:-no store line}" grep -q 'excluded_from_backup=true' <<<"$line"
  sleep 8
  shot 01-start
  # A light status bar shows as light pixels at the top of the dark start
  # screen; a dark one leaves almost none. A failed read leaves the count
  # empty, which fails the check.
  local lit
  lit=$(light_pixels "$OUT/shots/01-start.png" 2>/dev/null || true)
  check status_bar.light "$lit light pixels in the top 6% of the start screen" test "${lit:-0}" -ge 300
  endgroup

  group "the directory as the system holds it"
  # NSURLIsExcludedFromBackupKey is stored as this extended attribute.
  container=$(xcrun simctl get_app_container "$UDID" "$BUNDLE" data)
  local dir="$container/Library/Application Support/Tawara"
  ls -la "$dir" || true
  check backup.xattr "$(xattr -l "$dir" 2>&1 | head -2 | tr '\n' ' ')" \
    xattr -p com.apple.metadata:com_apple_backup_excludeItem "$dir"
  # The folder mode must be exactly 0700, as ios.rs sets it. The symbolic mode
  # is compared because the octal form omits the set-id and sticky bits.
  check dir.mode "$(stat -f '%Sp' "$dir")" test "$(stat -f '%Sp' "$dir")" = drwx------
  # The folder must have no access control list, which could grant access
  # regardless of the mode. ls -e prints each ACL entry on its own line, so a
  # single line means none (the backup attribute only adds @). The reads are
  # check arguments, not assignments, so an unreadable folder fails this check
  # instead of stopping the script under set -e.
  check dir.acl "$(ls -lde "$dir" 2>&1 | tr '\n' ' ' || true)" \
    test "$(ls -lde "$dir" 2>/dev/null | wc -l)" -eq 1
  endgroup

  group "to the background and back"
  # Seconds each step below waits for its log line; app switches on a hosted
  # simulator can be slow.
  local switch=90 front
  front=$(wc -l <"$err")
  xcrun simctl launch "$UDID" com.apple.Preferences >/dev/null
  # The application logs "cover added" only once the cover is on the window.
  # With no key window it logs a different line, which must not pass.
  line=$(wait_after "$err" "$front" '^TAWARA lifecycle: inactive; cover added$' "$switch" || true)
  check background.covered "${line:-no cover added line}" test -n "$line"
  # The winit window must be in the scene the manifest declares, or nothing is
  # shown. Read after the cover goes on, well after launch, so the order in
  # which the scene and window appeared does not matter.
  line=$(wait_after "$err" "$front" '^TAWARA scene: key window in scene=' 5 || true)
  check scene.window "${line:-no scene line}" grep -q 'scene=true$' <<<"$line"
  # `locked`, not `locking`: it is printed only once the worker has dropped the
  # session, which may happen after a running command finishes.
  line=$(wait_after "$err" "$front" '^TAWARA lifecycle: background; locked$' "$switch" || true)
  check background.locks "${line:-no lock line}" test -n "$line"
  sleep 3
  shot 02-settings-in-front
  front=$(wc -l <"$err")
  out=$(xcrun simctl launch "$UDID" "$BUNDLE")
  pid2=$(awk '{print $NF}' <<<"$out")
  line=$(wait_after "$err" "$front" '^TAWARA lifecycle: active; uncovered$' "$switch" || true)
  check return.uncovered "${line:-no uncover line}" test -n "$line"
  check return.same_process "pid $pid, then $pid2" test "$pid" = "$pid2"
  sleep 5
  shot 03-returned
  endgroup

  group "the copy path: options asserted, expiry observed"
  # The application copies only on user input, so the copy is made by a test
  # in crates/mobile/src/ios.rs run on this simulator. The test asserts the
  # pasteboard options (including local-only, which a simulator cannot
  # observe). The pasteboard is read with `simctl pbpaste` while the copy is
  # live and again after its lifetime.
  local test_exe clip=$OUT/clipboard.out clip_pid copied pasted after status=0 t=0
  test_exe=$(cd "$ROOT" && cargo test --release --locked -p tawara-mobile --lib \
    --target aarch64-apple-ios-sim --no-run --message-format=json | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") == "compiler-artifact" and m["profile"]["test"] and m["target"]["kind"] == ["lib"]:
        print(m["executable"])
')
  codesign --force --sign - --timestamp=none "$test_exe"
  xcrun simctl spawn "$UDID" "$test_exe" --exact ios::tests::a_copy_for_the_simulator_check \
    --nocapture --test-threads=1 >"$clip" 2>&1 &
  clip_pid=$!
  copied=$(wait_after "$clip" 0 '^TAWARA clipboard check: copied$' 120 || true)
  check clipboard.copied "${copied:-no copy line}" test -n "$copied"
  # A failed read must not be reported as an empty pasteboard.
  if pasted=$(wait_pasteboard "$UDID"); then
    check clipboard.on_pasteboard "pasteboard: '$pasted'" test "$pasted" = tawara-copy-check
  else
    record clipboard.on_pasteboard FAIL "simctl pbpaste failed"
  fi
  # The test ends a second after the lifetime. A hung test is killed here,
  # which fails clipboard.test, rather than waiting for the job timeout.
  while kill -0 "$clip_pid" 2>/dev/null && [ "$t" -lt 200 ]; do
    sleep 1
    t=$((t + 1))
  done
  kill "$clip_pid" 2>/dev/null || true
  wait "$clip_pid" || status=$?
  check clipboard.test "test exit $status" test "$status" = 0
  # Includes the test's assertions on the pasteboard options.
  check clipboard.ran "$(grep -o 'test result: .*' "$clip" | head -1)" grep -q 'test result: ok. 1 passed' "$clip"
  # The test exits only after the copy's lifetime, so the copy has expired by
  # now. A failed read must not pass as an empty pasteboard.
  if after=$(xcrun simctl pbpaste "$UDID" 2>/dev/null); then
    check clipboard.expired "pasteboard after its lifetime: '$after'" test -z "$after"
  else
    record clipboard.expired FAIL "simctl pbpaste failed"
  fi
  endgroup

  check no_panic "no panic on stderr" bash -c "! grep -q 'panicked' '$err'"
}

summary() {
  {
    echo "### iOS checks"
    echo
    echo "| check | result | detail |"
    echo "|---|---|---|"
    while IFS=$'\t' read -r name result detail; do
      echo "| $name | $result | ${detail//|/\\|} |"
    done <"$OUT/results.tsv"
  } >>"${GITHUB_STEP_SUMMARY:-/dev/null}"
}

evidence() {
  group "the application's stderr"
  cat "$OUT/app.err" 2>/dev/null || true
  endgroup
  summary
  echo "===== results ====="
  awk -F'\t' '{printf "CHECK %s %s: %s\n", $2, $1, $3}' "$OUT/results.tsv"
  if grep -q $'\tFAIL\t' "$OUT/results.tsv"; then
    echo "::error::the iOS checks have failures"
    exit 1
  fi
}

# cleanup: the EXIT trap of simulator. Shuts down and deletes the device this
# run created, then reports the results. The device is removed first because
# evidence exits 1 when a check failed. A failed delete is only a warning; the
# exit status is the run's own, or 1 if a check failed.
cleanup() {
  local rc=$?
  trap - EXIT
  if [ -n "${UDID:-}" ]; then
    xcrun simctl shutdown "$UDID" >/dev/null 2>&1 || true
    xcrun simctl delete "$UDID" >/dev/null || echo "::warning::could not delete the simulator $UDID"
  fi
  evidence
  exit "$rc"
}

case ${1:-} in
  device) device ;;
  simulator) simulator ;;
  *) die "usage: $0 device|simulator" ;;
esac
