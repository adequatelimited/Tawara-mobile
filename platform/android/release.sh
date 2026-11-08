#!/usr/bin/env bash
# Copyright (c) 2026 Adequate Systems HK Limited
# Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.
# Builds the Google Play App Bundle Tawara-X.Y.Z+N.aab. It builds
# libtawara_mobile.so for arm64-v8a and x86_64 with platform/android/tawara.sh
# build, links the manifest and resources with aapt2 into a base module, builds
# and validates the bundle with bundletool 1.18.3 using 16 KB page alignment,
# and signs it with the upload key. It never uploads.
#
# Run it from the commit tagged for the release. Keys and passwords come from
# the environment. Nothing secret is written into the repository, and passwords
# reach tools through the environment or a private file removed on exit, never
# on a command line.
#
#   ANDROID_HOME, or ANDROID_SDK_ROOT  the SDK, with build-tools 36.0.0,
#                             platforms/android-36 and NDK 29.0.14206865.
#                             Nothing is installed: a missing piece stops the
#                             script with the sdkmanager command that adds it.
#   JAVA_HOME                 a JDK 17 or later, for bundletool and jarsigner.
#                             Without it, java and jarsigner on PATH are used.
#   TAWARA_BUILD_NUMBER       required for a release: the committed counter. Raise it
#                             with 'platform/build-number.sh bump N', commit, then
#                             set this to N. A dry run uses the development number.
#   TAWARA_ANDROID_ICON_DIR   required for a release: a resource folder with
#                             mipmap-*/ic_launcher files, referenced in the
#                             manifest as @mipmap/ic_launcher. A dry run without
#                             it omits the icon with a warning.
#   TAWARA_UPLOAD_KEYSTORE    signing: the upload keystore.
#   TAWARA_UPLOAD_KEY_ALIAS   signing: the alias of the upload key in that keystore.
#   TAWARA_UPLOAD_STOREPASS   signing: the keystore password. Prompted for on a
#                             terminal when unset.
#   TAWARA_UPLOAD_KEYPASS     signing: the key password. Defaults to the keystore
#                             password.
#   TAWARA_ANDROID_APK=1      also builds a universal APK for sideloading, signed
#                             with the upload key (bundletool build-apks). Ignored
#                             in a dry run.
#   TAWARA_ANDROID_DRY_RUN=1  builds and checks everything and never signs.
#                             Release-only inputs (committed build number, icon,
#                             clean tree) become warnings, the upload key is not
#                             read, and outputs go to a separate folder.
#   TAWARA_ANDROID_OUT        output folder (default target/android-release, or
#                             target/android-dry-run for a dry run).
#   TAWARA_RELEASE_CACHE      cache for the bundletool jar, verified by SHA-256
#                             (default ~/.cache/tawara-release).
#
# When a signing variable is unset, the script names it, signs nothing, and exits 0
# with the unsigned bundle in the output folder.
set -Eeuo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# bn_number gives the build number used as versionCode.
# shellcheck source=../build-number.sh
. "$ROOT/platform/build-number.sh"

LIB=tawara_mobile
# Minimum and target API levels. TARGET_API must match tawara.sh.
MIN_API=26
TARGET_API=36
# Pinned tools: the build-tools providing aapt2 and zipalign, the platform jar
# the bundle is compiled against, and the NDK tawara.sh builds with.
BUILD_TOOLS_VERSION=36.0.0
PLATFORM=android-$TARGET_API
NDK_VERSION=29.0.14206865
# bundletool, pinned by version and by the SHA-256 of its GitHub release asset.
BUNDLETOOL_VERSION=1.18.3
BUNDLETOOL_SHA256=a099cfa1543f55593bc2ed16a70a7c67fe54b1747bb7301f37fdfd6d91028e29
BUNDLETOOL_URL=https://github.com/google/bundletool/releases/download/$BUNDLETOOL_VERSION/bundletool-all-$BUNDLETOOL_VERSION.jar
# The Rust targets tawara.sh builds.
TARGETS=(aarch64-linux-android x86_64-linux-android)

die() { echo "release.sh: error: $*" >&2; exit 1; }
warn() { echo "release.sh: warning: $*" >&2; }
note() { echo "release.sh: $*" >&2; }

# abi_of TARGET: the Android ABI for a Rust target.
abi_of() {
  case $1 in
    aarch64-linux-android) echo arm64-v8a ;;
    x86_64-linux-android) echo x86_64 ;;
    *) die "no Android ABI for $1" ;;
  esac
}

# sha256_of FILE: the SHA-256 digest of FILE.
sha256_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }

# ndk_host: the NDK prebuilt directory for this host, chosen as in tawara.sh.
# Requires NDK, set by check_sdk.
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

# package_version: the tawara-mobile version from cargo metadata.
package_version() {
  cargo metadata --locked --no-deps --format-version 1 \
    | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="tawara-mobile"))'
}

# locked_app_version: the tawara-app version in Cargo.lock. A release requires it
# to equal the tawara-mobile version.
locked_app_version() {
  awk -F'"' '/^name = "tawara-app"$/ { getline; if ($1 == "version = ") { print $2; exit } }' "$ROOT/Cargo.lock"
}

# check_sdk: verifies the pinned SDK components are present, without installing
# anything. Sets SDK, AAPT2, ZIPALIGN, APKSIGNER, JAR and NDK.
check_sdk() {
  SDK=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}
  [ -n "$SDK" ] || die "ANDROID_HOME is not set. Point it at an SDK with build-tools $BUILD_TOOLS_VERSION, $PLATFORM and NDK $NDK_VERSION"
  AAPT2=$SDK/build-tools/$BUILD_TOOLS_VERSION/aapt2
  ZIPALIGN=$SDK/build-tools/$BUILD_TOOLS_VERSION/zipalign
  APKSIGNER=$SDK/build-tools/$BUILD_TOOLS_VERSION/apksigner
  JAR=$SDK/platforms/$PLATFORM/android.jar
  NDK=$SDK/ndk/$NDK_VERSION
  [ -x "$AAPT2" ] || die "no aapt2 at $AAPT2. Install the pinned build-tools: sdkmanager --install 'build-tools;$BUILD_TOOLS_VERSION'"
  [ -x "$ZIPALIGN" ] && [ -x "$APKSIGNER" ] \
    || die "build-tools $BUILD_TOOLS_VERSION is incomplete at $SDK/build-tools/$BUILD_TOOLS_VERSION"
  [ -f "$JAR" ] || die "no $PLATFORM jar at $JAR. Install it: sdkmanager --install 'platforms;$PLATFORM'"
  [ -d "$NDK" ] || die "no NDK $NDK_VERSION at $NDK. Install it: sdkmanager --install 'ndk;$NDK_VERSION'"
}

# find_jdk: sets JAVA and JARSIGNER from JAVA_HOME or PATH; requires JDK 17 or later.
find_jdk() {
  if [ -n "${JAVA_HOME:-}" ]; then
    JAVA=$JAVA_HOME/bin/java
    JARSIGNER=$JAVA_HOME/bin/jarsigner
  else
    JAVA=$(command -v java || true)
    JARSIGNER=$(command -v jarsigner || true)
  fi
  [ -n "$JAVA" ] && [ -x "$JAVA" ] || die "no java. Set JAVA_HOME to a JDK 17 or later"
  [ -n "$JARSIGNER" ] && [ -x "$JARSIGNER" ] || die "no jarsigner. Set JAVA_HOME to a JDK 17 or later"
  local major
  # "21.0.12.1" gives 21, and the old "1.8.0_431" form gives 8.
  major=$("$JAVA" -version 2>&1 | sed -n 's/.*version "\([^"]*\)".*/\1/p' | sed -n 1p \
    | awk -F. '{ print ($1 == "1" ? $2 : $1) }') || true
  case $major in
    '' | *[!0123456789]*) die "cannot read the JDK version of $JAVA" ;;
  esac
  [ "$major" -ge 17 ] || die "$JAVA is JDK $major: bundletool and jarsigner need a JDK 17 or later"
}

# fetch_bundletool: sets BT_JAR to the bundletool jar, downloading it into the
# cache if needed and verifying it against BUNDLETOOL_SHA256 on every use. A
# mismatching jar is never run, and a mismatching download is removed.
fetch_bundletool() {
  local cache dir part got
  cache=${TAWARA_RELEASE_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/tawara-release}
  dir=$cache/bundletool-$BUNDLETOOL_VERSION
  BT_JAR=$dir/bundletool-all-$BUNDLETOOL_VERSION.jar
  if [ -f "$BT_JAR" ] && [ "$(sha256_of "$BT_JAR")" = "$BUNDLETOOL_SHA256" ]; then
    return 0
  fi
  mkdir -p "$dir"
  part=$BT_JAR.part
  rm -f "$part"
  note "fetching bundletool $BUNDLETOOL_VERSION from its GitHub release into $dir"
  curl -fsSL --proto '=https' --tlsv1.2 -o "$part" "$BUNDLETOOL_URL" \
    || { rm -f "$part"; die "cannot fetch $BUNDLETOOL_URL"; }
  got=$(sha256_of "$part")
  if [ "$got" != "$BUNDLETOOL_SHA256" ]; then
    rm -f "$part"
    die "bundletool's SHA-256 is $got, not the pinned $BUNDLETOOL_SHA256. The download was removed"
  fi
  mv "$part" "$BT_JAR"
}

# bt ARGS: runs bundletool with the JDK found by find_jdk.
bt() { "$JAVA" -jar "$BT_JAR" "$@"; }

# check_bundle_manifest FILE BUILD: verifies the base module manifest dumped by
# bundletool: target API, versionCode, not debuggable, backup off, backup rules
# and configChanges.
check_bundle_manifest() {
  local f=$1 build=$2
  grep -q "targetSdkVersion=\"$TARGET_API\"" "$f" \
    || die "the bundle's manifest does not target API $TARGET_API"
  grep -q "versionCode=\"$build\"" "$f" || die "the bundle's versionCode is not $build"
  if grep -q 'debuggable="true"' "$f"; then
    die "the bundle's manifest is debuggable"
  fi
  grep -q 'allowBackup="false"' "$f" || die "the bundle's manifest does not turn backup off"
  grep -q 'dataExtractionRules' "$f" || die "the bundle's manifest names no backup rules"
  # The mask of the twenty configuration changes the activity handles, in hex.
  grep -q 'configChanges="0xd800ffff"' "$f" \
    || die "the bundle's activity does not name the twenty configuration changes: configChanges must be 0xd800ffff"
}

# check_notices LISTING PREFIX WHAT: verifies the archive listing contains the
# notices under PREFIX/assets/ (base/ for a bundle, empty for an APK).
check_notices() {
  local prefix=$2 name
  for name in THIRD-PARTY-NOTICES.txt LICENSE.md; do
    grep -qx "${prefix}assets/$name" "$1" || die "$3 has no ${prefix}assets/$name: the release must carry the licence notices"
  done
}

# check_16k FILE: verifies every native library in FILE is stored uncompressed
# and its data starts on a 16 KB boundary, reading the local file headers.
check_16k() {
  python3 - "$1" <<'PY'
import struct, sys, zipfile
path = sys.argv[1]
problems = []
seen = 0
with zipfile.ZipFile(path) as z, open(path, "rb") as f:
    for info in z.infolist():
        name = info.filename
        if not (name.startswith("lib/") and name.endswith(".so")):
            continue
        seen += 1
        if info.compress_type != zipfile.ZIP_STORED:
            problems.append("%s is compressed" % name)
        f.seek(info.header_offset)
        header = f.read(30)
        if len(header) != 30 or header[:4] != b"PK\x03\x04":
            problems.append("%s has no local header" % name)
            continue
        name_len, extra_len = struct.unpack("<HH", header[26:30])
        start = info.header_offset + 30 + name_len + extra_len
        if start % 16384:
            problems.append("%s starts at byte %d" % (name, start))
if seen == 0:
    problems.append("no lib/*.so entries")
if problems:
    sys.exit("; ".join(problems))
print("%d native libraries, stored and on 16 KB boundaries" % seen)
PY
}

# check_apk APK BUILD: verifies the universal APK is signed, 16 KB aligned, and
# matches the bundle's version, target, ABIs, configChanges and notices.
check_apk() {
  local apk=$1 build=$2 badging
  "$APKSIGNER" verify "$apk" || die "apksigner does not verify $apk"
  "$ZIPALIGN" -c -P 16 4 "$apk" || die "zipalign -P 16 finds $apk unaligned"
  badging=$("$AAPT2" dump badging "$apk") || die "aapt2 cannot read $apk"
  case $badging in *"versionCode='$build'"*) ;; *) die "$apk: the versionCode is not $build" ;; esac
  case $badging in *"targetSdkVersion:'$TARGET_API'"*) ;; *) die "$apk: the target is not API $TARGET_API" ;; esac
  case $badging in *"application-debuggable"*) die "$apk is debuggable" ;; esac
  case $badging in
    *"native-code: 'arm64-v8a' 'x86_64'"*) ;;
    *) die "$apk does not carry arm64-v8a and x86_64 libraries" ;;
  esac
  "$AAPT2" dump xmltree --file AndroidManifest.xml "$apk" >"$WORK/apk-manifest.txt" \
    || die "aapt2 cannot dump the manifest of $apk"
  grep -Eq 'configChanges\(0x[0-9a-f]+\)=(\(type 0x11\))?0xd800ffff$' "$WORK/apk-manifest.txt" \
    || die "$apk: configChanges is not 0xd800ffff (20 names)"
  check_16k "$apk" >"$WORK/apk-16k.txt" || { cat "$WORK/apk-16k.txt" >&2; die "$apk is not 16 KB aligned"; }
  unzip -Z1 "$apk" >"$WORK/apk-entries.txt" || die "unzip cannot list $apk"
  check_notices "$WORK/apk-entries.txt" "" "$apk"
  note "$(basename "$apk"): $(cat "$WORK/apk-16k.txt")"
}

# sign_bundle IN OUT: signs IN into OUT with the upload key, then verifies OUT.
# Passwords reach jarsigner through the environment, never its command line.
sign_bundle() {
  TAWARA_SIGN_STOREPASS=$store_pass TAWARA_SIGN_KEYPASS=$key_pass "$JARSIGNER" \
    -keystore "$TAWARA_UPLOAD_KEYSTORE" -storepass:env TAWARA_SIGN_STOREPASS \
    -keypass:env TAWARA_SIGN_KEYPASS -digestalg SHA-256 -signedjar "$2" "$1" \
    "$TAWARA_UPLOAD_KEY_ALIAS" >"$WORK/jarsigner.txt" 2>&1 \
    || { cat "$WORK/jarsigner.txt" >&2; die "jarsigner could not sign $1"; }
  local out
  out=$("$JARSIGNER" -verify "$2" 2>&1) || { printf '%s\n' "$out" >&2; die "jarsigner cannot verify $2"; }
  case $out in
    *"jar verified."*) ;;
    *) printf '%s\n' "$out" >&2; die "$2 does not verify as signed" ;;
  esac
}

# universal_apk BUNDLE OUT: writes the universal APK of BUNDLE to OUT, signed with
# the upload key. bundletool reads the passwords from private files.
universal_apk() {
  bt build-apks --bundle="$1" --output="$WORK/universal.apks" --mode=universal \
    --ks="$TAWARA_UPLOAD_KEYSTORE" --ks-key-alias="$TAWARA_UPLOAD_KEY_ALIAS" \
    --ks-pass=file:"$PRIV/store" --key-pass=file:"$PRIV/key" --overwrite \
    >"$WORK/build-apks.txt" 2>&1 \
    || { cat "$WORK/build-apks.txt" >&2; die "bundletool could not build the universal APK"; }
  unzip -p "$WORK/universal.apks" universal.apk >"$2" \
    || die "the APK set has no universal.apk"
}

main() {
  cd "$ROOT"
  local dry=0 sign=0 want_apk=0 build version app_version icon="" NAME unset_list="" tdir
  local aab apk=""
  case ${TAWARA_ANDROID_DRY_RUN:-0} in
    0) ;;
    1) dry=1 ;;
    *) die "TAWARA_ANDROID_DRY_RUN is '$TAWARA_ANDROID_DRY_RUN': use 1 for a dry run, or leave it unset" ;;
  esac
  case ${TAWARA_ANDROID_APK:-0} in
    0) ;;
    1) want_apk=1 ;;
    *) die "TAWARA_ANDROID_APK is '$TAWARA_ANDROID_APK': use 1 to make the universal APK, or leave it unset" ;;
  esac
  if [ "$dry" = 1 ]; then
    note "dry run: builds and checks everything; the release-only inputs only warn"
  fi

  # Release inputs, checked before any tool runs. A dry run only warns about the
  # ones it can do without.
  if [ "$dry" = 0 ]; then
    [ -n "${TAWARA_BUILD_NUMBER:-}" ] || die "TAWARA_BUILD_NUMBER is not set. A release takes the committed build number: run 'platform/build-number.sh bump N', commit it, then set TAWARA_BUILD_NUMBER=N"
    [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ] \
      && die "tracked files differ from the commit. A release is built from the commit it is tagged on"
  elif [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
    warn "tracked files differ from the commit: a release would refuse this tree"
  fi
  build=$(bn_number) || die "TAWARA_BUILD_NUMBER does not match platform/build-number (see the message above)"
  if [ -n "${TAWARA_ANDROID_ICON_DIR:-}" ]; then
    icon=$TAWARA_ANDROID_ICON_DIR
    [ -d "$icon" ] || die "TAWARA_ANDROID_ICON_DIR is not a folder"
    ls "$icon"/mipmap-*/ic_launcher* >/dev/null 2>&1 \
      || die "$icon has no mipmap-*/ic_launcher file"
  elif [ "$dry" = 0 ]; then
    die "TAWARA_ANDROID_ICON_DIR is not set. A release needs the app icon: set it to a resource folder with mipmap-*/ic_launcher files"
  else
    warn "TAWARA_ANDROID_ICON_DIR is not set: this dry run leaves the app icon out"
  fi
  version=$(package_version)
  app_version=$(locked_app_version)
  [ "$version" = "$app_version" ] \
    || die "tawara-mobile is $version and tawara-app in Cargo.lock is $app_version: a release needs them equal"
  NAME=Tawara-$version+$build
  note "version $version, build $build ($NAME)"

  check_sdk
  find_jdk
  fetch_bundletool
  [ "$(bt version)" = "$BUNDLETOOL_VERSION" ] || die "bundletool at $BT_JAR is not $BUNDLETOOL_VERSION"

  # Signing. A dry run never signs, builds no universal APK and does not read the
  # upload key. Otherwise unset variables are named and the bundle stays unsigned.
  [ -n "${TAWARA_UPLOAD_KEYSTORE:-}" ] || unset_list="$unset_list TAWARA_UPLOAD_KEYSTORE"
  [ -n "${TAWARA_UPLOAD_KEY_ALIAS:-}" ] || unset_list="$unset_list TAWARA_UPLOAD_KEY_ALIAS"
  store_pass=${TAWARA_UPLOAD_STOREPASS:-}
  if [ "$dry" = 1 ]; then
    note "dry run: nothing is signed and no universal APK is made"
    want_apk=0
  elif [ -n "$unset_list" ]; then
    note "signing SKIPPED, because these are unset:$unset_list. Set them to sign."
  else
    [ -f "$TAWARA_UPLOAD_KEYSTORE" ] || die "TAWARA_UPLOAD_KEYSTORE is not a file"
    if [ -z "$store_pass" ] && [ -t 0 ]; then
      printf 'Password for the upload keystore: ' >&2
      IFS= read -r -s store_pass
      echo >&2
    fi
    if [ -z "$store_pass" ]; then
      note "signing SKIPPED: TAWARA_UPLOAD_STOREPASS is unset and there is no terminal to ask for it"
    else
      sign=1
    fi
  fi
  key_pass=${TAWARA_UPLOAD_KEYPASS:-$store_pass}

  OUT=${TAWARA_ANDROID_OUT:-}
  if [ -z "$OUT" ]; then
    if [ "$dry" = 1 ]; then OUT=$ROOT/target/android-dry-run; else OUT=$ROOT/target/android-release; fi
  fi
  case $OUT in
    /*) ;;
    *) OUT=$ROOT/$OUT ;;
  esac
  WORK=$OUT/work
  rm -rf "$WORK" "$OUT/$NAME.aab" "$OUT/$NAME-unsigned.aab" "$OUT/$NAME.apk" "$OUT/SHA256SUMS"
  mkdir -p "$WORK"
  if [ "$sign" = 1 ] || [ "$want_apk" = 1 ]; then
    PRIV=$(mktemp -d "${TMPDIR:-/tmp}/tawara-release.XXXXXX")
    trap 'rm -rf "$PRIV"' EXIT
    # Passwords go to private files for bundletool, removed on exit.
    (umask 077 && printf '%s\n' "$store_pass" >"$PRIV/store" && printf '%s\n' "$key_pass" >"$PRIV/key")
  fi

  # Release libraries for each target. tawara.sh checks the ANativeActivity_onCreate
  # export and 16 KB LOAD alignment.
  note "building the libraries with platform/android/tawara.sh build"
  ANDROID_HOME=$SDK RUNNER_TEMP=$WORK/runner bash "$HERE/tawara.sh" build
  # Cargo's target directory, which CARGO_TARGET_DIR may override.
  tdir=$(RUNNER_TEMP=$WORK/runner bash "$HERE/tawara.sh" target-dir) \
    || die "tawara.sh cannot name the target directory"
  TC=$NDK/toolchains/llvm/prebuilt/$(ndk_host)/bin
  [ -x "$TC/llvm-strip" ] || die "no llvm-strip at $TC"

  # The committed manifest, with the icon reference added to its single
  # <application> line when an icon is given.
  local manifest_src=$ROOT/platform/android/AndroidManifest.xml
  [ "$(grep -c '^[[:space:]]*<application$' "$manifest_src")" = 1 ] \
    || die "$manifest_src must have one <application> line to name the icon in"
  mkdir -p "$WORK/manifest"
  if [ -n "$icon" ]; then
    sed 's|^\([[:space:]]*\)<application$|\1<application android:icon="@mipmap/ic_launcher"|' \
      "$manifest_src" >"$WORK/manifest/AndroidManifest.xml"
  else
    cp "$manifest_src" "$WORK/manifest/AndroidManifest.xml"
  fi

  # Compile the resources and link them in protocol-buffer format against the
  # pinned platform, with the build number and version.
  local flat=$WORK/flat
  mkdir -p "$flat"
  "$AAPT2" compile --dir "$ROOT/platform/android/res" -o "$flat"
  if [ -n "$icon" ]; then
    "$AAPT2" compile --dir "$icon" -o "$flat"
  fi
  "$AAPT2" link --proto-format -o "$WORK/link.zip" -I "$JAR" \
    --manifest "$WORK/manifest/AndroidManifest.xml" -R "$flat"/*.flat \
    --min-sdk-version "$MIN_API" --target-sdk-version "$TARGET_API" \
    --version-code "$build" --version-name "$version" --replace-version

  # The base module: the proto manifest under manifest/, the resource table, res/,
  # the libraries under lib/<abi>/ (stored, not deflated), and the notices under
  # assets/.
  rm -rf "$WORK/proto"
  mkdir -p "$WORK/proto"
  unzip -q "$WORK/link.zip" -d "$WORK/proto" || die "aapt2 made no zip"
  [ -f "$WORK/proto/AndroidManifest.xml" ] && [ -f "$WORK/proto/resources.pb" ] && [ -d "$WORK/proto/res" ] \
    || die "aapt2 made no proto manifest, resource table and res/"
  local module=$WORK/module t abi
  rm -rf "$module"
  mkdir -p "$module/manifest" "$module/assets" "$module/lib"
  mv "$WORK/proto/AndroidManifest.xml" "$module/manifest/AndroidManifest.xml"
  mv "$WORK/proto/resources.pb" "$module/resources.pb"
  mv "$WORK/proto/res" "$module/res"
  cp "$ROOT/THIRD-PARTY-NOTICES.txt" "$ROOT/LICENSE.md" "$module/assets/"
  for t in "${TARGETS[@]}"; do
    abi=$(abi_of "$t")
    mkdir -p "$module/lib/$abi"
    "$TC/llvm-strip" --strip-debug -o "$module/lib/$abi/lib$LIB.so" "$tdir/$t/release/lib$LIB.so"
  done
  rm -f "$WORK/base.zip"
  (cd "$module" && zip -q -X -D -0 -r "$WORK/base.zip" lib)
  (cd "$module" && zip -q -X -D -r "$WORK/base.zip" manifest resources.pb res assets)

  # The bundle, configured so that APKs built from it align native libraries to
  # 16 KB pages.
  cat >"$WORK/BundleConfig.json" <<'JSON'
{
  "optimizations": {
    "uncompressNativeLibraries": {
      "enabled": true,
      "alignment": "PAGE_ALIGNMENT_16K"
    }
  }
}
JSON
  bt build-bundle --modules="$WORK/base.zip" --output="$WORK/unsigned.aab" \
    --config="$WORK/BundleConfig.json" --overwrite
  bt validate --bundle="$WORK/unsigned.aab" >"$WORK/validate.txt" 2>&1 \
    || { cat "$WORK/validate.txt" >&2; die "bundletool validate refused the bundle"; }
  bt dump manifest --bundle="$WORK/unsigned.aab" >"$WORK/bundle-manifest.xml"
  bt dump config --bundle="$WORK/unsigned.aab" >"$WORK/bundle-config.json"
  check_bundle_manifest "$WORK/bundle-manifest.xml" "$build"
  grep -q 'PAGE_ALIGNMENT_16K' "$WORK/bundle-config.json" \
    || die "the bundle's configuration does not ask for PAGE_ALIGNMENT_16K"
  unzip -Z1 "$WORK/unsigned.aab" >"$WORK/bundle-entries.txt" || die "unzip cannot list the bundle"
  check_notices "$WORK/bundle-entries.txt" base/ "the bundle"
  note "the bundle validates; its manifest targets API $TARGET_API with versionCode $build, is not debuggable, has the 20 configuration changes, and asks for 16 KB pages"

  if [ "$sign" = 1 ]; then
    aab=$OUT/$NAME.aab
    sign_bundle "$WORK/unsigned.aab" "$aab"
    bt validate --bundle="$aab" >"$WORK/validate-signed.txt" 2>&1 \
      || { cat "$WORK/validate-signed.txt" >&2; die "bundletool validate refused the signed bundle"; }
    note "signed $(basename "$aab") with the upload key '$TAWARA_UPLOAD_KEY_ALIAS'; jarsigner verifies it"
  else
    aab=$OUT/$NAME-unsigned.aab
    cp "$WORK/unsigned.aab" "$aab"
  fi

  if [ "$want_apk" = 1 ]; then
    if [ "$sign" = 1 ]; then
      apk=$OUT/$NAME.apk
      universal_apk "$aab" "$apk"
      check_apk "$apk" "$build"
    else
      note "universal APK SKIPPED: it is signed with the upload key, which is not set (see above)"
    fi
  fi

  # SHA-256 sums of the outputs, verified before printing.
  if [ -n "$apk" ]; then
    (cd "$OUT" && shasum -a 256 "$(basename "$aab")" "$(basename "$apk")" >SHA256SUMS)
  else
    (cd "$OUT" && shasum -a 256 "$(basename "$aab")" >SHA256SUMS)
  fi
  (cd "$OUT" && shasum -a 256 -c SHA256SUMS >/dev/null) || die "SHA256SUMS does not check"
  cat "$OUT/SHA256SUMS"

  echo "release.sh: made $aab"
  [ -n "$apk" ] && echo "release.sh: made $apk"
  if [ "$sign" = 1 ]; then
    echo "release.sh: signed bundle: $aab"
  else
    echo "release.sh: no signed bundle exists: the upload key was not set"
  fi
  cat <<TXT
Not run by this script: validate and upload the bundle by hand in Play Console,
to the internal testing track, which enrols Play App Signing for the first upload.
TXT
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
