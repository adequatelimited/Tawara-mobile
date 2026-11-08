#!/usr/bin/env bash
# Copyright (c) 2026 Adequate Systems HK Limited
# Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.
# Builds and tests Tawara for Android:
#
#   platform/android/tawara.sh build      # libtawara_mobile.so, arm64-v8a and x86_64
#   platform/android/tawara.sh apk        # the APK, signed with a throwaway debug key
#   platform/android/tawara.sh emulator   # boot an emulator, install, run the checks
#   platform/android/tawara.sh selftest   # the crash and destroy checks, on logged lines
#   platform/android/tawara.sh target-dir # the directory build leaves the libraries in
#
# No Java, Kotlin or Gradle: the activity is android.app.NativeActivity, and
# the APK is linked with aapt2, then zipped, aligned and signed by hand.
#
# The application prints `TAWARA ...` lines on stderr, which android-activity
# sends to logcat under the tag RustStdoutStderr. This script prints
# `CHECK PASS|FAIL <name>: <detail>` lines and a table in the step summary,
# and fails if any check failed. A count that is reported but not gated is
# printed as `CHECK NOTE <name>: <detail>` and never fails the job. The
# system's lines about the application's activity and process (`trace`) are
# printed after the first start, after the asset-path check and at the end,
# so a start-up that ends says why in the job log.
set -Eeuo pipefail

PKG=com.adequatelimited.tawara
ACTIVITY=android.app.NativeActivity
LIB=tawara_mobile
MIN_API=26
TARGET_API=36
TARGETS=(aarch64-linux-android x86_64-linux-android)
IMG="system-images;android-36;google_apis;x86_64"
NDK_PIN=29.0.14206865
SERIAL=emulator-5554
TAG=RustStdoutStderr
# The logcat buffers the checks read: the application and the system, the
# crashes, and the events of activity starts, relaunches and process ends.
LOG_BUFFERS=main,system,crash,events
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# bn_number gives the build number used as versionCode.
# shellcheck source=../build-number.sh
. "$ROOT/platform/build-number.sh"
OUT=${RUNNER_TEMP:-/tmp}/tawara-android
# build, apk and emulator need the SDK (need_sdk); selftest runs without it.
SDK=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}
[ -z "$SDK" ] || export PATH="$SDK/platform-tools:$SDK/emulator:$SDK/cmdline-tools/latest/bin:$PATH"
LOG=$OUT/logcat.txt
mkdir -p "$OUT"

group() { echo "::group::$*"; }
endgroup() { echo "::endgroup::"; }
die() { echo "::error::$*"; exit 1; }
need_sdk() { [ -n "$SDK" ] || die "ANDROID_HOME is not set"; }

# ndk_paths: the pinned NDK, not whichever 29.x is installed, and its
# toolchain for this host. Installs the NDK when it is missing.
ndk_paths() {
  NDK=$SDK/ndk/$NDK_PIN
  if [ ! -d "$NDK" ]; then
    # `yes` dies of SIGPIPE when sdkmanager exits; only sdkmanager's status counts.
    (yes 2>/dev/null || true) | sdkmanager --install "ndk;$NDK_PIN" >/dev/null
  fi
  TC=$NDK/toolchains/llvm/prebuilt/$(ndk_host)/bin
  [ -x "$TC/clang" ] || die "no NDK clang at $TC"
}

# ndk_host: the NDK's prebuilt directory for this machine. On macOS,
# darwin-arm64 is used when present; otherwise darwin-x86_64, whose clang is a
# universal binary.
ndk_host() {
  case $(uname -s) in
    Linux) echo linux-x86_64 ;;
    Darwin)
      if [ -d "$NDK/toolchains/llvm/prebuilt/darwin-arm64" ]; then
        echo darwin-arm64
      else
        echo darwin-x86_64
      fi
      ;;
    *) die "no NDK host directory for $(uname -s)" ;;
  esac
}

abi_of() {
  case $1 in
    aarch64-linux-android) echo arm64-v8a ;;
    x86_64-linux-android) echo x86_64 ;;
  esac
}

# target_dir: cargo's target directory for this workspace, which
# CARGO_TARGET_DIR or build.target-dir can move away from target/.
target_dir() {
  (cd "$ROOT" && cargo metadata --locked --no-deps --format-version 1) \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])'
}

# --------------------------------------------------------------------- build
build() {
  ndk_paths
  echo "NDK $NDK"
  cd "$ROOT"
  local tdir
  tdir=$(target_dir) || die "cargo metadata gave no target directory"
  for t in "${TARGETS[@]}"; do
    local u=${t//-/_}
    # Upper case by tr, not ${u^^}, which macOS's bash 3.2 does not have.
    local U
    U=$(printf '%s' "$u" | tr 'a-z' 'A-Z')
    export "CARGO_TARGET_${U}_LINKER=$TC/${t}${MIN_API}-clang"
    export "CC_${u}=$TC/${t}${MIN_API}-clang" "CXX_${u}=$TC/${t}${MIN_API}-clang++"
    export "AR_${u}=$TC/llvm-ar" "RANLIB_${u}=$TC/llvm-ranlib"
    # Remove any earlier library so a stale one cannot pass the checks below.
    local so=$tdir/$t/release/lib$LIB.so
    rm -f "$so"
    group "cargo rustc --crate-type cdylib --target $t"
    cargo rustc --locked --release -p tawara-mobile --lib --crate-type cdylib --target "$t"
    endgroup
    [ -f "$so" ] || die "$so was not built"
    "$TC/llvm-readelf" --dyn-syms -W "$so" | grep -qE ' ANativeActivity_onCreate$' \
      || die "$so does not export ANativeActivity_onCreate"
    # 16 KB pages: every LOAD segment must be aligned to at least 2**14.
    "$TC/llvm-readelf" -lW "$so" | awk '$1=="LOAD"{print $NF}' | sort -u | while read -r align; do
      [ $((align)) -ge 16384 ] || die "$so: a LOAD segment aligned to $align"
    done
    ls -la "$so"
  done
}

# ----------------------------------------------------------------------- apk
apk() {
  ndk_paths
  cd "$ROOT"
  local bt jar w=$OUT/apk build badging tdir
  # The development number, or a store build's committed counter; a refused
  # number ends the script here.
  build=$(bn_number)
  bt=$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)
  # aapt2 links against API 36's jar, pinned rather than the newest installed,
  # because only that platform defines the assetsPaths attribute. The target
  # API level stays TARGET_API. The platform is installed when missing, and
  # the build stops rather than compile against another one.
  jar=$SDK/platforms/android-36/android.jar
  if [ ! -f "$jar" ]; then
    # `yes` dies of SIGPIPE when sdkmanager exits; only sdkmanager's status counts.
    (yes 2>/dev/null || true) | sdkmanager --install "platforms;android-36" >/dev/null
  fi
  [ -f "$jar" ] || die "no android-36 platform in $SDK/platforms (sdkmanager 'platforms;android-36')"
  echo "build-tools $bt; platform $jar; build number $build"
  rm -rf "$w"
  mkdir -p "$w/root/lib" "$w/res"
  tdir=$(target_dir) || die "cargo metadata gave no target directory"
  for t in "${TARGETS[@]}"; do
    local abi
    abi=$(abi_of "$t")
    mkdir -p "$w/root/lib/$abi"
    "$TC/llvm-strip" --strip-debug -o "$w/root/lib/$abi/lib$LIB.so" "$tdir/$t/release/lib$LIB.so"
  done
  # A throwaway key for this run only: this APK is for testing, never for
  # distribution.
  keytool -genkeypair -noprompt -keystore "$w/debug.keystore" -storepass android -keypass android \
    -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
    -dname "CN=Android Debug,O=Android,C=US" >/dev/null 2>&1
  "$bt/aapt2" compile --dir platform/android/res -o "$w/res"
  # Debuggable, so the checks can read the app's directory with run-as.
  "$bt/aapt2" link -o "$w/unsigned.apk" -I "$jar" --manifest platform/android/AndroidManifest.xml \
    -R "$w"/res/*.flat --min-sdk-version "$MIN_API" --target-sdk-version "$TARGET_API" --debug-mode \
    --version-code "$build" --version-name "$(cargo metadata --no-deps --format-version 1 \
      | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="tawara-mobile"))')"
  # Stored, not deflated, and 16 KB-aligned: with extractNativeLibs="false"
  # the system maps the libraries in place.
  (cd "$w/root" && zip -q -0 -r -D -X "../unsigned.apk" lib)
  "$bt/zipalign" -f -P 16 4 "$w/unsigned.apk" "$w/aligned.apk"
  "$bt/apksigner" sign --ks "$w/debug.keystore" --ks-pass pass:android --key-pass pass:android \
    --ks-key-alias androiddebugkey --v4-signing-enabled false --out "$OUT/tawara.apk" "$w/aligned.apk"
  "$bt/apksigner" verify "$OUT/tawara.apk"
  "$bt/zipalign" -c -P 16 4 "$OUT/tawara.apk"
  badging=$("$bt/aapt2" dump badging "$OUT/tawara.apk")
  # The versionCode must be the build number. A case pattern, not grep -q, so
  # no pipe closes early.
  case $badging in
    *"versionCode='$build'"*) ;;
    *) die "$OUT/tawara.apk: the versionCode is not $build" ;;
  esac
  printf '%s\n' "$badging" | grep -E "^package:|targetSdk|native-code|uses-permission"
  "$bt/aapt2" dump xmltree --file AndroidManifest.xml "$OUT/tawara.apk" >"$OUT/manifest.txt"
  ls -la "$OUT/tawara.apk"
}

# ------------------------------------------------------------------ helpers
a() { adb -s "$SERIAL" "$@"; }
ash() { adb -s "$SERIAL" shell "$@" | tr -d '\r'; }

record() { # record NAME PASS|FAIL|NOTE DETAIL
  printf '%s\t%s\t%s\n' "$1" "$2" "${3:-}" >>"$OUT/results.tsv"
  echo "CHECK $2 $1: ${3:-}"
}
check() { # check NAME DETAIL COMMAND...: PASS when the command succeeds
  local name=$1 detail=$2
  shift 2
  if "$@"; then record "$name" PASS "$detail"; else record "$name" FAIL "$detail"; fi
}
mark() { wc -l <"$LOG"; }
app_lines() { tail -n +"$(($1 + 1))" "$LOG" | grep -E "$TAG: TAWARA " | sed -E "s/.*$TAG: //" || true; }
wait_log() { # wait_log MARK ERE SECONDS: prints the first matching application line
  local t=0 line
  while :; do
    line=$(app_lines "$1" | grep -E -m1 "$2" || true)
    [ -n "$line" ] && { echo "$line"; return 0; }
    [ "$t" -ge "$3" ] && return 1
    sleep 1
    t=$((t + 1))
  done
}
app_pid() { ash pidof "$PKG" || true; }
# same_pid PID: the application runs under PID. An empty PID never passes.
same_pid() { [ -n "$1" ] && [ "$(app_pid)" = "$1" ]; }
resumed() { ash dumpsys activity activities | grep -m1 -E 'topResumedActivity=|ResumedActivity:' || true; }
wait_sys() { # wait_sys MARK ERE SECONDS: prints the first line since MARK that matches, of any tag
  local t=0 line
  while :; do
    line=$(tail -n +"$(($1 + 1))" "$LOG" | grep -E -m1 "$2" || true)
    [ -n "$line" ] && { echo "$line"; return 0; }
    [ "$t" -ge "$3" ] && return 1
    sleep 1
    t=$((t + 1))
  done
}
# no_sys MARK ERE: no line since MARK matches. grep -c reads every line, so
# tail is never cut off by a closed pipe that pipefail would report.
no_sys() {
  local n
  n=$(tail -n +"$(($1 + 1))" "$LOG" | grep -cE "$2" || true)
  [ "$n" = 0 ]
}
overlay_state() { ash cmd overlay list | grep -E "\] $1\$" | head -n 1 || true; }
# trace MARK [END]: the log lines after MARK, up to END or the end of the log,
# that show how the application started and ended, each cut to 300
# characters. At most the last 200 are printed, since a relaunch and a process
# end come last. Kept: system records that name $PKG; overlay commits; and,
# from the application's own processes, activity events, asset updates, fatal
# messages and TAWARA lines.
trace() {
  local slice=$OUT/trace-slice.txt kept=$OUT/trace-kept.txt n
  awk -v a="$1" -v b="${2:-}" 'NR > a && (b == "" || NR <= b)' "$LOG" >"$slice" || true
  if ! awk -v pkg="$PKG" '
    FNR == NR {
      if (index($0, pkg)) {
        if ($6 ~ /^(RustStdoutStderr|ActivityThread|AndroidRuntime):$/) own[$3] = 1
        if (match($0, /am_proc_(start|died): \[[0-9]+,[0-9]+,/)) {
          split(substr($0, RSTART, RLENGTH), f, ",")
          own[f[2]] = 1
        }
      }
      next
    }
    {
      tag = $6
      sub(/:$/, "", tag)
      event = (tag ~ /^(am|wm)_/)
      named = (tag ~ /^(ActivityTaskManager|ActivityManager|WindowManager|ActivityThread|AndroidRuntime|DEBUG|lowmemorykiller|OverlayManager|PackageManager)$/)
      if (index($0, pkg) && (event || named)) print
      else if (tag == "commit_sys_config_file" && /\[overlays,/) print
      else if (!($3 in own)) next
      else if (event) print
      else if (tag == "ActivityThread" && /assets (added|removed)/) print
      else if (tag == "libc" && $5 == "F") print
      else if (tag == "RustStdoutStderr" && / RustStdoutStderr: TAWARA /) print
    }
  ' "$slice" "$slice" >"$kept"; then
    echo "::warning::trace: awk failed, so the lines below may be incomplete"
  fi
  n=$(awk 'END { print NR }' "$kept")
  if [ "$n" -gt 200 ]; then echo "($((n - 200)) earlier lines not shown)"; fi
  tail -n 200 "$kept" | cut -c1-300
  rm -f "$slice" "$kept"
}
# crash_lines FROM PID...: one line per crash after log line FROM (0 for the
# whole log): 'own' or 'other', a tab, and the logged text.
#
# A crash that names its process is judged by that name alone, since recorded
# PIDs span reboots and can be reused. The application has one process, named
# after the package. An unnamed crash is the application's when its PID was
# recorded by the run or wrote a TAWARA line; otherwise it is 'other'.
#
# The line after a FATAL EXCEPTION is 'Process: NAME, PID: N' or 'PID: N';
# without it, the FATAL line's PID column is used. A 'Fatal signal' line names
# the crashing process as 'pid N (name)', which is used instead of the PID
# column, because another process can write the line. That name is the
# kernel's, at most 15 characters, so it is matched against the package, the
# package's last 15 characters, and the names read from /proc/PID/comm
# (COMMS). A false match can fail the check but never hide a crash. A 'Fatal
# signal' that names no process counts as the application's. A Rust panic
# under the application's tag is the application's.
crash_lines() {
  local from=$1
  shift
  awk -v pkg="$PKG" -v tag="$TAG" -v pids=" $* " -v comms=" $COMMS " -v from="$from" '
    BEGIN {
      n = 0; pending = 0; panic = tag ": thread .* panicked"
      # The kernel keeps the last 15 characters of a process name.
      tail = length(pkg) > 15 ? substr(pkg, length(pkg) - 14) : pkg
    }
    index($0, tag ": TAWARA ") { app[$3] = 1 }
    # The lines up to FROM only say which PIDs wrote TAWARA lines.
    NR <= from { next }
    index($0, "FATAL EXCEPTION") {
      pending = ++n
      text[n] = substr($0, index($0, "FATAL EXCEPTION"))
      name[n] = ""
      pid[n] = $3
      where[n] = ", pid " $3 ", no process name logged"
      unknown[n] = 0
      next
    }
    pending && match($0, /(Process: [^,]+, )?PID: [0-9]+/) {
      s = substr($0, RSTART, RLENGTH)
      i = index(s, "PID: ")
      pid[pending] = substr(s, i + 5)
      if (i > 1) {
        name[pending] = substr(s, 10, i - 12)
        where[pending] = " in " name[pending] ", pid " pid[pending]
      } else {
        where[pending] = ", pid " pid[pending] ", no process name logged"
      }
      pending = 0
      next
    }
    index($0, "Fatal signal") {
      pending = 0
      text[++n] = substr($0, index($0, "Fatal signal"))
      name[n] = ""
      pid[n] = ""
      where[n] = ", no process logged"
      unknown[n] = 1
      if (match($0, /pid [0-9]+( \([^)]*\))?/)) {
        s = substr($0, RSTART + 4, RLENGTH - 4)
        i = index(s, " (")
        if (i > 0) {
          pid[n] = substr(s, 1, i - 1)
          name[n] = substr(s, i + 2, length(s) - i - 2)
          where[n] = ""
          unknown[n] = 0
        } else {
          pid[n] = s
          where[n] = ", no process name logged"
        }
      }
      next
    }
    match($0, panic) {
      pending = 0
      text[++n] = substr($0, index($0, tag))
      name[n] = pkg
      pid[n] = $3
      where[n] = ""
      unknown[n] = 0
      next
    }
    { pending = 0 }
    END {
      for (i = 1; i <= n; i++) {
        if (unknown[i] || name[i] == pkg || name[i] == tail || (name[i] != "" && index(comms, " " name[i] " "))) mine = 1
        else if (name[i] != "") mine = 0
        else mine = (pid[i] != "" && ((pid[i] in app) || index(pids, " " pid[i] " ") > 0))
        s = text[i] where[i]
        printf "%s\t%s\n", (mine ? "own" : "other"), substr(s, 1, 240)
      }
    }
  ' "$LOG"
}
# no_crash_check PID...: records the no_crash check. A crash of the
# application fails it; another process's crash is noted and passes. An empty
# or missing log fails it, since it cannot show that nothing crashed.
no_crash_check() {
  local crashes own other
  if [ ! -s "$LOG" ]; then
    record no_crash FAIL "the log is empty or missing, so a crash cannot be ruled out"
    return 0
  fi
  crashes=$(crash_lines 0 "$@")
  own=$(grep '^own' <<<"$crashes" | cut -f2- | head -3 | tr '\n' ' ' || true)
  other=$(grep '^other' <<<"$crashes" | cut -f2- | head -1 || true)
  if [ -n "$own" ]; then
    record no_crash FAIL "$own"
  elif [ -n "$other" ]; then
    record no_crash PASS "no crash of the application; another process's crash is noted, not counted: $other"
  else
    record no_crash PASS "no crash or panic in the log"
  fi
  grep '^other' <<<"$crashes" | cut -f2- | sed 's/^/note: not counted, another process: /' || true
}
tag_lines() { tail -n +"$(($1 + 1))" "$LOG" | grep -E "$TAG" || true; } # since MARK
# The TAWARA lines one process logged, read from the device's buffer so the
# final exit line does not depend on adb having written the file yet.
device_lines() { # device_lines PID
  ash logcat -d -v threadtime --pid="$1" | grep -E "$TAG: TAWARA " | sed -E "s/.*$TAG: //" || true
}
wait_device() { # wait_device PID ERE SECONDS: prints the first matching line of that process
  local t=0 line
  while :; do
    line=$(device_lines "$1" | grep -E -m1 "$2" || true)
    [ -n "$line" ] && { echo "$line"; return 0; }
    [ "$t" -ge "$3" ] && return 1
    sleep 1
    t=$((t + 1))
  done
}
# aborts MARK PID...: the application's SIGABRTs since MARK, one per line.
# Ownership is decided by crash_lines, so this agrees with no_crash.
aborts() {
  crash_lines "$@" | awk -F '\t' '$1 == "own" && $2 ~ /Fatal signal 6|SIGABRT/ { print $2 }' || true
}
# COMMS: space-separated kernel names of the application's processes in this
# run, matched by crash_lines.
COMMS=
# note_comm PID: adds /proc/PID/comm to COMMS. Called on each application
# process while it lives.
note_comm() {
  local c
  [ -n "$1" ] || return 0
  c=$(ash cat "/proc/$1/comm" 2>/dev/null || true)
  [ -n "$c" ] || return 0
  case " $COMMS " in *" $c "*) ;; *) COMMS="${COMMS:+$COMMS }$c" ;; esac
}
# 'Don't keep activities' is the global setting always_finish_activities. The
# framework reads it only at boot (ActivityManagerService.retrieveSettings),
# so each change writes the setting and reboots the emulator, and the
# framework's field is read back with dumpsys. The binder method
# setAlwaysFinish is not used, since its transaction number is image-specific.
#
# KEEP_WAS is the value before the first change: empty while untouched, `null`
# when never set. KEEP_CHANGED is yes from the first write until the value is
# restored, so the EXIT trap restores it at most once.
KEEP_WAS=
KEEP_CHANGED=no

# keep_write 1|0|delete: writes the setting, then reboots so the framework
# reads it. Returns 1 when the emulator does not come back.
keep_write() {
  case $1 in
    delete) ash settings delete global always_finish_activities >/dev/null || return 1 ;;
    *) ash settings put global always_finish_activities "$1" || return 1 ;;
  esac
  # The settings provider persists a change after up to two seconds
  # (SettingsState.WRITE_SETTINGS_DELAY_MILLIS), so wait before rebooting.
  sleep 3
  reboot_and_wait
}
# dont_keep_activities 1|0: turns the option on or off, through a reboot.
dont_keep_activities() {
  KEEP_CHANGED=yes
  keep_write "$1"
}
# Restores the option to its original value; does nothing once restored.
restore_keep() {
  local failed=0
  [ "$KEEP_CHANGED" = yes ] || return 0
  case $KEEP_WAS in
    '') return 0 ;;
    null) keep_write delete || failed=1 ;;
    *) keep_write "$KEEP_WAS" || failed=1 ;;
  esac
  if [ "$failed" -eq 0 ]; then KEEP_CHANGED=no; fi
  return "$failed"
}
# For the EXIT trap: restores the option, or reports the failure.
restore_at_exit() {
  restore_keep >/dev/null 2>&1 || echo "::error::'Don't keep activities' could not be put back to $KEEP_WAS"
}

# Reboots the emulator and waits until the framework answers again, then wakes
# the screen and starts a new logcat stream. Returns 1 when the emulator does
# not come back within about ten minutes.
reboot_and_wait() {
  local before="" after="" t=0
  before=$(ash cat /proc/sys/kernel/random/boot_id || true)
  # Without the old boot id the wait could end before the reboot begins.
  [ -n "$before" ] || return 1
  # adb may report the closing connection as an error; the boot id shows
  # whether the reboot happened.
  a reboot || true
  until [ -n "$after" ] && [ "$after" != "$before" ]; do
    [ "$t" -lt 300 ] || return 1
    sleep 3
    t=$((t + 3))
    after=$(ash cat /proc/sys/kernel/random/boot_id || true)
  done
  until [ "$(ash getprop sys.boot_completed || true)" = 1 ]; do
    [ "$t" -lt 600 ] || return 1
    sleep 3
    t=$((t + 3))
  done
  until a shell pm path android >/dev/null 2>&1; do
    [ "$t" -lt 600 ] || return 1
    sleep 2
    t=$((t + 2))
  done
  ash input keyevent KEYCODE_WAKEUP || true
  ash wm dismiss-keyguard || true
  restart_log
}
# A reboot ends the logcat stream. The new stream appends to the same file, so
# earlier marks still hold. The device buffer is cleared first, so the crash
# checks read the application's lines and not the system's boot.
restart_log() {
  if [ -f "$OUT/logcat.pid" ]; then kill "$(cat "$OUT/logcat.pid")" 2>/dev/null || true; fi
  a logcat -b "$LOG_BUFFERS" -c || { echo "::error::the log could not be cleared after the reboot"; return 1; }
  a logcat -b "$LOG_BUFFERS" -v threadtime >>"$LOG" 2>&1 &
  echo $! >"$OUT/logcat.pid"
}
# 1 while the framework's always-finish field is on, 0 otherwise.
runtime_keep() {
  ash dumpsys activity processes | grep -c '^  mAlwaysFinishActivities=true$' || true
}
# destroy_state PID: the event log since the last clear, made just before
# Back. Prints 'destroyed' when this component's activity was destroyed with
# reason always-finish before PID died, 'died' when PID died first, and 'none'
# otherwise. Fields (AOSP): wm_destroy_activity is User, Token, Task ID,
# Component Name, Reason; am_proc_died is User, PID, Process Name, OomAdj,
# ProcState. wm_destroy_activity carries no PID; the clear before Back ties it
# to this run.
destroy_state() {
  ash logcat -b events -d -v threadtime | destroy_parse "$1"
}
# destroy_parse PID: destroy_state's verdict on the event-log lines on stdin.
destroy_parse() {
  awk -v pid="$1" -v comp="$PKG/$ACTIVITY" '
    state != "" { next }
    /wm_destroy_activity/ && match($0, /\[.*\]/) {
      n = split(substr($0, RSTART + 1, RLENGTH - 2), f, ",")
      if (n == 5 && f[4] == comp && f[5] == "always-finish") state = "destroyed"
      next
    }
    /am_proc_died/ && match($0, /\[.*\]/) {
      n = split(substr($0, RSTART + 1, RLENGTH - 2), f, ",")
      if (n >= 2 && f[2] == pid) state = "died"
    }
    END { print (state == "" ? "none" : state) }
  '
}

set_ini() {
  local f=$1 k=$2 v=$3
  if grep -q "^$k=" "$f"; then sed -i "s|^$k=.*|$k=$v|" "$f"; else echo "$k=$v" >>"$f"; fi
}

emulator_up() {
  group "KVM"
  echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' \
    | sudo tee /etc/udev/rules.d/99-kvm4all.rules >/dev/null
  sudo udevadm control --reload-rules
  sudo udevadm trigger --name-match=kvm
  ls -l /dev/kvm || die "/dev/kvm missing"
  endgroup

  group "SDK packages"
  (yes 2>/dev/null || true) | sdkmanager --licenses >/dev/null 2>&1 || true
  sdkmanager --install platform-tools emulator "$IMG" >"$OUT/sdkmanager.txt" 2>&1 \
    || { tail -30 "$OUT/sdkmanager.txt"; die "sdkmanager failed"; }
  # The emulator needs desktop libraries even with -no-window.
  sudo apt-get update -q >/dev/null
  sudo apt-get install -y -q --no-install-recommends libpulse0 libnss3 libxcomposite1 \
    libxcursor1 libxdamage1 libxi6 libxtst6 libxkbfile1 libgl1 libegl1 libasound2t64 \
    libbsd0 libxkbcommon-x11-0 libx11-xcb1 >/dev/null
  emulator -version | sed -n 1p
  endgroup

  group "AVD and boot"
  echo no | avdmanager create avd --force -n tawara -k "$IMG"
  local avd_dir
  avd_dir=$(avdmanager list avd | sed -n 's/^ *Path: \(.*tawara\.avd\)$/\1/p' | sed -n 1p)
  [ -n "$avd_dir" ] && [ -f "$avd_dir/config.ini" ] || { avdmanager list avd; die "cannot find the AVD"; }
  export ANDROID_AVD_HOME
  ANDROID_AVD_HOME=$(dirname "$avd_dir")
  local cfg=$avd_dir/config.ini
  set_ini "$cfg" hw.keyboard no
  set_ini "$cfg" hw.ramSize 4096
  set_ini "$cfg" hw.cpu.ncore 4
  set_ini "$cfg" disk.dataPartition.size 6G
  set_ini "$cfg" hw.lcd.width 1080
  set_ini "$cfg" hw.lcd.height 2400
  set_ini "$cfg" hw.lcd.density 420
  nohup emulator -avd tawara -port "${SERIAL#emulator-}" -no-window -no-audio -no-boot-anim \
    -no-snapshot -gpu swiftshader_indirect -accel on -camera-back none -camera-front none \
    >"$OUT/emulator.txt" 2>&1 &
  echo $! >"$OUT/emulator.pid"
  adb start-server >/dev/null
  timeout 300 adb -s "$SERIAL" wait-for-device || { tail -50 "$OUT/emulator.txt"; die "no adb device"; }
  local t=0
  until [ "$(ash getprop sys.boot_completed 2>/dev/null)" = 1 ]; do
    kill -0 "$(cat "$OUT/emulator.pid")" 2>/dev/null || { tail -80 "$OUT/emulator.txt"; die "the emulator exited"; }
    [ "$t" -ge 600 ] && { tail -80 "$OUT/emulator.txt"; die "no boot after 600 s"; }
    sleep 5
    t=$((t + 5))
  done
  until a shell pm path android >/dev/null 2>&1; do sleep 2; done
  echo "booted after about ${t} s"
  endgroup

  group "device"
  ash settings put global window_animation_scale 0
  ash settings put global transition_animation_scale 0
  ash settings put global animator_duration_scale 0
  ash settings put global stay_on_while_plugged_in 7
  ash settings put system screen_off_timeout 2147483647
  ash input keyevent KEYCODE_WAKEUP
  ash wm dismiss-keyguard || true
  echo "sdk=$(ash getprop ro.build.version.sdk) abi=$(ash getprop ro.product.cpu.abilist)"
  # The events buffer holds the activity starts, relaunches and process ends
  # (wm_*, am_*) that `trace` prints.
  a logcat -b "$LOG_BUFFERS" -c || true
  a logcat -b "$LOG_BUFFERS" -v threadtime >"$LOG" 2>&1 &
  echo $! >"$OUT/logcat.pid"
  endgroup
}

emulator_down() {
  a emu kill >/dev/null 2>&1 || true
  [ -f "$OUT/logcat.pid" ] && kill "$(cat "$OUT/logcat.pid")" 2>/dev/null || true
}

# ------------------------------------------------------------------- checks
summary() {
  {
    echo "### Android checks"
    echo
    echo "| check | result | detail |"
    echo "|---|---|---|"
    while IFS=$'\t' read -r name result detail; do
      echo "| $name | $result | ${detail//|/\\|} |"
    done <"$OUT/results.tsv"
  } >>"${GITHUB_STEP_SUMMARY:-/dev/null}"
}

# Not `emulator`: a function of that name would shadow the SDK's emulator
# program, which emulator_up runs.
on_emulator() {
  # Start with empty results so an earlier run's FAIL cannot fail this one.
  : >"$OUT/results.tsv"
  emulator_up
  # Restores 'Don't keep activities' even when a check fails part-way.
  trap 'restore_at_exit; summary; emulator_down' EXIT

  group "the manifest"
  # The backup rules.
  check manifest.allowBackup "android:allowBackup is false" \
    grep -qE 'allowBackup\(0x[0-9a-f]+\)=(false|\(type 0x12\)0x0)' "$OUT/manifest.txt"
  check manifest.dataExtractionRules "android:dataExtractionRules names the rules" \
    grep -q 'android:dataExtractionRules' "$OUT/manifest.txt"
  # An empty task affinity keeps other applications' activities out of the task.
  check manifest.taskAffinity "android:taskAffinity is empty" \
    grep -qE 'taskAffinity\(0x[0-9a-f]+\)=""' "$OUT/manifest.txt"
  # The configChanges mask must hold all twenty names: the eighteen of API 35
  # plus assetsPaths and resourcesUnused of API 36. A missing name lets Android
  # recreate the activity, which ends the application. Every mask aapt2 read
  # is joined on one line, so a second mask fails the check.
  local changes
  changes=$(awk 'match($0, /configChanges\(0x[0-9a-f]+\)=.*/) {
    s = s (s == "" ? "" : "; ") substr($0, RSTART)
  } END { print (s == "" ? "no configChanges" : s) }' "$OUT/manifest.txt")
  check manifest.configChanges "want 0xd800ffff (20 names): $changes" \
    grep -qE '^configChanges\(0x[0-9a-f]+\)=(\(type 0x11\))?0xd800ffff$' <<<"$changes"
  endgroup

  group "install and start"
  # Marked before the install, so the trace holds the overlay update that can
  # follow it.
  local m m0 pid
  m0=$(mark)
  a install -r -g "$OUT/tawara.apk"
  m=$(mark)
  ash am start -W -n "$PKG/$ACTIVITY"
  check start "the application starts in its private directory" \
    wait_log "$m" '^TAWARA start: private directory .*/no_backup$' 60
  sleep 5
  pid=$(app_pid)
  note_comm "$pid"
  check running "a process, pid $pid, which the kernel names '${COMMS:-unread}'" test -n "$pid"
  check resumed "the activity is in front: $(resumed)" grep -q "$PKG" <<<"$(resumed)"
  endgroup

  group "first launch: the system's account"
  # The system's lines since before the install, and the overlay list.
  local mt
  mt=$(mark)
  trace "$m0" "$mt"
  ash cmd overlay list | head -n 200 || true
  endgroup

  group "FLAG_SECURE"
  # dumpsys lists FLAG_SECURE as SECURE.
  local flags
  flags=$(ash dumpsys window windows | grep -A30 "$PKG/$ACTIVITY" | grep -m1 -E 'fl=|flags=' || true)
  check flag_secure "${flags//$'\t'/ }" grep -qE 'SECURE' <<<"$flags"
  endgroup

  group "the store's directory"
  # no_backup/ is the store's parent. android.rs makes it exactly 0700, since
  # the library refuses a parent as open as files/ (0771). toybox's ls prints
  # the mode and then a space.
  local listing
  listing=$(ash run-as "$PKG" ls -ld no_backup 2>&1 || true)
  check no_backup.private "$listing" grep -qE '^drwx------ ' <<<"$listing"
  endgroup

  group "an asset-path change keeps the activity"
  # Toggling the navigation-mode overlay changes every running application's
  # asset paths. The manifest declares assetsPaths, so the activity must not be
  # relaunched. Requires the activity in front before the toggle and the
  # update delivered to it; then the process must stay, the activity must stay
  # in front without a relaunch, and leaving the screen must still lock. An
  # image without the gestural navigation overlay fails here.
  local ov=com.android.internal.systemui.navbar.gestural was op undo p0 front m1 m2 toggled
  was=$(overlay_state "$ov")
  p0=$(app_pid)
  front=$(resumed)
  if [ -z "$was" ]; then
    record assets.same_process FAIL "no overlay $ov on this image, so nothing was toggled"
  elif [ -z "$p0" ] || ! grep -q "$PKG" <<<"$front"; then
    record assets.same_process FAIL "not running and in front before the toggle (pid '$p0'), so nothing was toggled: $front"
    echo "The system's account since the first launch, at the end of this log, says why."
  else
    case $was in
      *'[x]'*) op=disable undo=enable ;;
      *) op=enable undo=disable ;;
    esac
    m1=$(mark)
    toggled=$(ash cmd overlay "$op" --user 0 "$ov" 2>&1 | tr '\t\n' '  ' || true)
    check assets.delivered "the system updated the application's asset paths after overlay $op${toggled:+ ($toggled)}" \
      wait_sys "$m1" "ActivityThread: ApplicationInfo updating for $PKG" 20
    sleep 10
    check assets.same_process "pid $(app_pid), was $p0, after overlay $op" same_pid "$p0"
    check assets.resumed "in front after overlay $op: $(resumed)" grep -q "$PKG" <<<"$(resumed)"
    check assets.no_relaunch "no relaunch of the activity after overlay $op" \
      no_sys "$m1" "wm_relaunch(_resume)?_activity: .*$PKG"
    m2=$(mark)
    trace "$m1" "$m2"
    ash input keyevent KEYCODE_HOME
    check assets.locks "the lock on leaving the screen after overlay $op" \
      wait_log "$m2" '^TAWARA lifecycle: suspended; locking$' 30
    ash cmd overlay "$undo" --user 0 "$ov" >/dev/null 2>&1 || true
    local t=0
    until [ "$(overlay_state "$ov")" = "$was" ] || [ "$t" -ge 10 ]; do
      sleep 1
      t=$((t + 1))
    done
    if [ "$(overlay_state "$ov")" = "$was" ]; then
      echo "overlay $ov put back: $was"
    else
      echo "::warning::overlay $ov not put back after overlay $undo: $(overlay_state "$ov"), was $was"
    fi
    # The Home and Back checks start with the activity in front.
    ash am start -W -n "$PKG/$ACTIVITY" >/dev/null
    sleep 5
  fi
  endgroup

  group "Home: suspended and locked, then back"
  m=$(mark)
  ash input keyevent KEYCODE_HOME
  check home.locks "the lock on leaving the screen" \
    wait_log "$m" '^TAWARA lifecycle: suspended; locking$' 30
  sleep 2
  ash am start -W -n "$PKG/$ACTIVITY"
  sleep 5
  check home.same_process "the process lives on (pid $(app_pid), was $pid)" same_pid "$pid"
  check home.resumed "in front again: $(resumed)" grep -q "$PKG" <<<"$(resumed)"
  endgroup

  group "Back: to the background, not finished"
  m=$(mark)
  ash input keyevent KEYCODE_BACK
  check back.locks "Back takes the task to the background, which locks" \
    wait_log "$m" '^TAWARA lifecycle: suspended; locking$' 30
  sleep 2
  ash am start -W -n "$PKG/$ACTIVITY"
  sleep 5
  check back.same_process "the process lives on (pid $(app_pid), was $pid)" same_pid "$pid"
  endgroup

  group "Back and the lock before the exit"
  # The first Back keeps the activity and the process. With 'Don't keep
  # activities' on, Back destroys the activity; winit then ends its event loop,
  # run_in returns and the process ends (exit.ended). The 'TAWARA exit' line
  # says whether the lock finished first, which is a race, so several fresh
  # processes are measured. A run counts only when its process logged the lock
  # after Back and the event log holds this component's always-finish destroy
  # before the process died. A SIGABRT in this window fails. exit.destroyed
  # separates 'not destroyed' from 'destroyed before the lock finished';
  # exit.lock is a reported count, not a gate: a Destroy within about 100 ms
  # of backgrounding can end the process before the lock completes in
  # App::update. Ending the process leaves no secret in memory, and the
  # library's store writes are atomic.
  local m0 mr kept runtime restored state alive runs=5 run rpid line suspended exit_line gone aborted
  local pids="" started=0 locked=0 finished=0 ended=0 destroyed=0
  m0=$(mark)
  ash input keyevent KEYCODE_BACK
  sleep 5
  echo "$TAG lines after Back from the foreground:"
  tag_lines "$m0"
  # Check the first process for SIGABRT before the reboot below.
  aborted=$(aborts "$m0" "$pid")
  KEEP_WAS=$(ash settings get global always_finish_activities)
  dont_keep_activities 1 || die "the emulator did not come back after the reboot that turns 'Don't keep activities' on"
  mr=$(mark)
  kept=$(ash settings get global always_finish_activities)
  check exit.setting "'Don't keep activities' reads $kept, was $KEEP_WAS" test "$kept" = 1
  # The framework's field should now be on. The runs continue either way,
  # since exit.destroyed checks each run's destroy on its own.
  runtime=$(runtime_keep)
  check exit.runtime "the framework's field is on after the reboot: $runtime line(s) of mAlwaysFinishActivities=true, 1 expected" \
    test "$runtime" = 1
  for ((run = 1; run <= runs; run++)); do
    ash am force-stop "$PKG"
    m=$(mark)
    ash am start -W -n "$PKG/$ACTIVITY"
    if ! wait_log "$m" '^TAWARA start: ' 60 >/dev/null; then
      echo "run $run: the relaunched process did not start"
      continue
    fi
    started=$((started + 1))
    sleep 5
    rpid=$(app_pid)
    pids="$pids $rpid"
    note_comm "$rpid"
    m=$(mark)
    # Cleared before Back, so the destroy read afterwards is this run's.
    ash logcat -b events -c || die "the event log could not be cleared"
    ash input keyevent KEYCODE_BACK
    suspended=no exit_line='no TAWARA exit line' gone=no
    wait_device "$rpid" '^TAWARA lifecycle: suspended; locking$' 30 >/dev/null && suspended=yes
    line=$(wait_device "$rpid" '^TAWARA exit: ' 30 || true)
    [ -n "$line" ] && exit_line=$line
    # Long enough for a SIGABRT during exit to be logged.
    sleep 10
    [ -z "$(app_pid)" ] && gone=yes
    state=no-process
    [ -n "$rpid" ] && state=$(destroy_state "$rpid" || true)
    # A new PID means the application restarted, so the destroy is not this run's.
    alive=$(app_pid)
    if [ -n "$alive" ] && [ "$alive" != "$rpid" ]; then state=restarted; fi
    [ "$state" = destroyed ] && destroyed=$((destroyed + 1))
    # Only a run whose activity was destroyed counts as an exit measurement.
    if [ "$state" = destroyed ]; then
      [ "$suspended" = yes ] && locked=$((locked + 1))
      [ "$suspended" = yes ] && [ "$exit_line" = 'TAWARA exit: lock done' ] && finished=$((finished + 1))
      [ "$gone" = yes ] && ended=$((ended + 1))
    fi
    echo "run $run: pid $rpid; activity $state; locking logged: $suspended; $exit_line; process ended: $gone"
    echo "$TAG lines after Back with 'Don't keep activities' on:"
    tag_lines "$m"
  done
  # Check these processes for SIGABRT before the restoring reboot.
  # shellcheck disable=SC2086 # Split on purpose: one argument per pid.
  [ -z "$pids" ] || aborted=$(printf '%s\n%s\n' "$aborted" "$(aborts "$mr" $pids)")
  if ! restore_keep; then
    echo "::error::'Don't keep activities' could not be put back; the EXIT trap tries again"
  fi
  check exit.started "$started of $runs relaunched processes started" test "$started" = "$runs"
  check exit.destroyed "$destroyed of $runs runs: the event log holds the activity's destroy after Back (always-finish), and the run's process had not died before it" \
    test "$destroyed" = "$runs"
  check exit.locks "$locked of $runs runs logged the lock after Back (only destroyed runs count)" test "$locked" = "$runs"
  check exit.ended "$ended of $runs processes ended after Back (only destroyed runs count)" test "$ended" = "$runs"
  # Reported as a NOTE, which does not fail the job.
  record exit.lock NOTE "$finished of $runs runs: TAWARA exit: lock done (only destroyed runs count; reported, not gated)"
  # The count first, since the lines are cut short for the table.
  aborted=$(printf '%s\n' "$aborted" | grep -v '^$' || true)
  local naborts
  naborts=$(grep -c . <<<"$aborted" || true)
  if [ "$naborts" = 0 ]; then
    aborted="no SIGABRT or Fatal signal 6 from the application since the first Back"
  else
    aborted="$naborts of the application's aborts since the first Back: $(tr '\n' ' ' <<<"$aborted" | cut -c1-300)"
  fi
  check exit.no_sigabrt "$aborted" test "$naborts" = 0
  restored=$(ash settings get global always_finish_activities || true)
  check exit.restored "'Don't keep activities' reads $restored, as it was ($KEEP_WAS)" test "$restored" = "$KEEP_WAS"
  endgroup

  # Only the application's crashes fail this check; see crash_lines.
  # shellcheck disable=SC2086 # Split on purpose: one argument per pid.
  no_crash_check "$pid" $pids

  group "the system's account since the first launch"
  trace "$mt"
  endgroup

  group "the application's lines"
  grep -E "$TAG: TAWARA " "$LOG" | sed -E "s/.*$TAG: //" || true
  endgroup
  ! grep -q $'\tFAIL\t' "$OUT/results.tsv"
}

# selftest: runs the crash parser, no_crash, the abort check and destroy_parse
# on recorded log lines, and fails when a result differs. The first five
# 'Fatal signal' lines are real emulator output: application SIGABRTs under
# the kernel's 15-character process name. The other three are synthetic: the
# system server's abort; an abort naming pid 1940 but written by pid 1000; and
# an abort under the package's first 15 characters, which counts as the
# application's only once that name is in COMMS. The event-log lines are the
# launcher's destroy, the application's always-finish destroy, and its
# process's death.
selftest() {
  local dir=$OUT/selftest failed=0 got
  mkdir -p "$dir"
  # Local LOG, OUT and COMMS shadow the run's, so no run result is touched.
  local LOG=$dir/logcat.txt
  local OUT=$dir
  local COMMS=
  : >"$OUT/results.tsv"
  cat >"$LOG" <<'LOGCAT'
10-12 23:45:29.642  2060  2132 F libc    : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 2132 (hwuiTask0), pid 2060 (elimited.tawara)
10-12 23:45:47.758  3651  3676 F libc    : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 3676 (hwuiTask0), pid 3651 (elimited.tawara)
10-12 23:46:06.085  4026  4051 F libc    : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 4051 (hwuiTask0), pid 4026 (elimited.tawara)
10-12 23:46:22.372  4110  4135 F libc    : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 4135 (hwuiTask0), pid 4110 (elimited.tawara)
10-12 23:46:39.773  4318  4345 F libc    : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 4345 (hwuiTask1), pid 4318 (elimited.tawara)
10-12 23:46:40.600   655   695 F libc    : Fatal signal 6 (SIGABRT), code -6 (SI_TKILL) in tid 695 (system_server), pid 655 (system_server)
10-12 23:46:41.118  1000  1000 F libc    : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 2007 (hwuiTask0), pid 1940 (elimited.tawara)
10-12 23:46:42.200  5000  5021 F libc    : Fatal signal 6 (SIGABRT), code -1 (SI_QUEUE) in tid 5021 (hwuiTask0), pid 5000 (com.adequatelim)
LOGCAT
  got=$(crash_lines 0 2060 3651 4026 4110 4318 | cut -f1 | tr '\n' ' ')
  selfcheck selftest.verdicts "own own own own own other own other " "$got" \
    "the five aborts of the run, the system server's, one written by another process, one under a name the run did not read"
  COMMS="elimited.tawara com.adequatelim"
  got=$(crash_lines 0 2060 | tail -1 | cut -f1)
  COMMS=
  selfcheck selftest.kernel_name own "$got" "an abort under a name read from /proc for the application"
  got="$(aborts 0 2060 | grep -c . || true) $(aborts 6 2060 | grep -c . || true)"
  selfcheck selftest.aborts "6 1" "$got" "the application's aborts in the whole log, and after line 6"
  no_crash_check 2060 >/dev/null
  got=$(tail -1 "$OUT/results.tsv" | cut -f2)
  LOG=$dir/other.txt
  sed -n 6p "$dir/logcat.txt" >"$LOG"
  no_crash_check 2060 >/dev/null
  got="$got $(tail -1 "$OUT/results.tsv" | cut -f2)"
  selfcheck selftest.no_crash "FAIL PASS" "$got" "no_crash on the whole log, and on the system server's abort alone"
  local ev=$dir/events.txt
  cat >"$ev" <<'EVENTS'
10-13 21:01:41.313   648   700 I wm_destroy_activity: [0,101443555,2,com.google.android.apps.nexuslauncher/.NexusLauncherActivity,always-finish]
10-13 21:01:46.850   648   700 I wm_destroy_activity: [0,85398786,9,com.adequatelimited.tawara/android.app.NativeActivity,always-finish]
10-13 21:01:46.878   648   744 I am_proc_died: [0,1880,com.adequatelimited.tawara,900,18]
EVENTS
  got="$(destroy_parse 1880 <"$ev") $(destroy_parse 99999 <"$ev")"
  got="$got $(sed -n 3p "$ev" | cat - "$ev" | destroy_parse 1880)"
  got="$got $(sed -e '2!d' -e 's/always-finish/finish-imm:transit/' "$ev" | destroy_parse 1880)"
  got="$got $(sed -n 1p "$ev" | destroy_parse 1880)"
  selfcheck selftest.destroy "destroyed destroyed died none none" "$got" \
    "the run's destroy, the same lines for a PID they do not name, the process dead first, another reason, another activity"
  return "$failed"
}
# selfcheck NAME WANT GOT DETAIL: prints a CHECK line, and sets selftest's
# failed when GOT is not WANT.
selfcheck() {
  if [ "$3" = "$2" ]; then
    echo "CHECK PASS $1: $4: $3"
  else
    echo "CHECK FAIL $1: $4: got '$3', want '$2'"
    failed=1
  fi
}

case ${1:-} in
  build) need_sdk; build ;;
  apk) need_sdk; apk ;;
  emulator) need_sdk; on_emulator ;;
  selftest) selftest ;;
  target-dir) target_dir ;;
  *) die "usage: $0 build|apk|emulator|selftest|target-dir" ;;
esac
