# Android platform glue

Tawara on Android is `crates/mobile` loaded as `libtawara_mobile.so` by
`android.app.NativeActivity`: no Java, Kotlin or Gradle. The application ID
is the `package` in `AndroidManifest.xml`.

- `AndroidManifest.xml`: NativeActivity, `hasCode="false"`, uncompressed
  16 KB-aligned native libraries, `INTERNET`, `CAMERA` for the QR scanner
  (requested at run time; `android.hardware.camera` and
  `android.hardware.camera.any` are not required, so a device without a
  camera can install it), the twenty configuration changes handled in place
  (the eighteen of API 35 plus `assetsPaths` and `resourcesUnused` of API 36,
  so `configChanges` reads `0xd800ffff`), Back delivered as a key event, and
  the backup rules: `android:allowBackup="false"` and `dataExtractionRules`.
- `res/xml/data_extraction_rules.xml`: every domain excluded from cloud
  backup and from device transfer. The store also lives in `no_backup/`.
- `tawara.sh`:
  - `build`: the library for arm64-v8a and x86_64 with the release profile,
    the NDK version pinned in the script and the host's prebuilt toolchain.
  - `apk`: linked with aapt2 against API 36's platform jar (the first to
    define `assetsPaths`), targeting API 36, with the build number from
    `platform/build-number.sh` as `versionCode`; aligned and signed with a
    throwaway debug key, for testing only. The platform is installed with
    sdkmanager when missing.
  - `emulator`: the checks CI runs (`.github/workflows/mobile.yml`), plus the
    system's lines about the application after the first start, after the
    asset-path check and at the end, so a start-up that ends says why.
  - `selftest`: the crash, abort and destroy parsers on recorded log lines,
    with no SDK or device; CI runs it before the emulator.

  The emulator has no camera (`-camera-back none -camera-front none`), so a
  scan there is refused at once and the camera path is not exercised.
- `release.sh`: the Google Play App Bundle, `Tawara-X.Y.Z+N.aab`, and an
  optional universal APK; see "The Google Play bundle" below.

What the Rust side does (`crates/mobile/src/android.rs`): `FLAG_SECURE` on
the whole window, the store in `no_backup/`, the clipboard through JNI with
every copy marked sensitive, the share sheet (an `ACTION_SEND` chooser) and
Back through JNI, and the lock when the system suspends the application.

The QR scanner (`crates/mobile/src/android/scan.rs`) reads the back camera
through Camera2 and an NDK `AImageReader`, and decodes the luminance with
`rqrr` (`crates/mobile/src/qr.rs`). It requests the camera permission through
JNI and polls for the answer, since a NativeActivity gets no permission
callback. It passes each frame's preview to the application through
`preview_frame`, at most ten a second, rotated upright for the display, and
the application draws it. The camera stops when the scan ends, when the
application moves to the background, when the window loses focus, and when
the application ends.

To try a build on a phone: take `tawara.apk` from a local
`platform/android/tawara.sh build && platform/android/tawara.sh apk` (with
`ANDROID_HOME` set and the NDK installed by it), then
`adb install -r tawara.apk`.

## The Google Play bundle

`release.sh` makes `Tawara-X.Y.Z+N.aab` from the commit tagged for the
upload. It runs on a release machine; CI never runs it.

It builds both libraries with `tawara.sh build`. It links the manifest and the
resources with build-tools 36.0.0's `aapt2 link --proto-format` against
`platforms/android-36`, with `--target-sdk-version 36`, the build number as
`versionCode`, the version as `versionName`, and never `--debug-mode`. The notices
go under the base module's `assets/`. bundletool 1.18.3 builds the bundle and
validates it with 16 KB page alignment for the native libraries, and `jarsigner`
signs it with the upload key, since `apksigner` cannot sign a bundle. The bundletool
jar is fetched once from its GitHub release, into `~/.cache/tawara-release` (or
`TAWARA_RELEASE_CACHE`), and checked against its SHA-256 each time it is used.
Apart from that download, nothing is sent anywhere, and nothing is uploaded or
validated with Google.

### Release steps

1. Gather the items under "What the publisher supplies", and have the SDK
   pieces and a JDK 17 or later in place. The script checks for them and
   never installs them: a missing piece stops it, and the message gives the
   `sdkmanager` line that adds it.
2. Set the version, raise the build number, regenerate the notices, commit, and tag
   the commit `vX.Y.Z+N`, as steps 2 to 5 of the iOS list in `platform/ios/README.md`
   say. The same version and the same counter serve both stores.
3. Optional: a dry run, `TAWARA_ANDROID_DRY_RUN=1 platform/android/release.sh`. It
   builds and checks everything, treats the release-only inputs as warnings, and
   writes to `target/android-dry-run`. It never signs and makes no universal APK,
   whatever the upload variables say.
4. Check out the tag, then run `platform/android/release.sh` with the variables below
   set, `TAWARA_BUILD_NUMBER=N` among them. The keystore password is typed at the
   prompt, or set in `TAWARA_UPLOAD_STOREPASS`.
5. Read the output. `SHA256SUMS` lists what was made, and the status lines say
   whether a signed bundle exists. Without a signature the bundle is named
   `Tawara-X.Y.Z+N-unsigned.aab`.
6. Upload the signed bundle by hand in Play Console, to the internal testing
   track, which enrols Play App Signing at the first upload.

The script does not run the emulator checks on the universal APK.

### Environment

None of these values is written into a file the repository keeps. Passwords reach
the tools through the environment or a private file that is removed on exit, never
on a command line.

- `ANDROID_HOME` (or `ANDROID_SDK_ROOT`): the SDK, with build-tools 36.0.0,
  `platforms/android-36` and NDK 29.0.14206865.
- `JAVA_HOME`: a JDK 17 or later, for bundletool and jarsigner. Without it, the
  `java` and `jarsigner` on the `PATH` are used. An older JDK stops the script.
- `TAWARA_BUILD_NUMBER`: required for a release, the committed counter (see the iOS
  list above). A dry run takes the development number.
- `TAWARA_ANDROID_ICON_DIR`: required for a release. A resource folder holding
  `mipmap-*/ic_launcher` files, which the manifest names as `@mipmap/ic_launcher`. A
  dry run without it leaves the icon out, with a warning.
- `TAWARA_UPLOAD_KEYSTORE` and `TAWARA_UPLOAD_KEY_ALIAS`: the upload keystore, and the
  alias of its key. Without either, or without a keystore password and a terminal to
  ask for one, the script names what is unset, signs nothing and exits 0, with the
  unsigned bundle in the output folder.
- `TAWARA_UPLOAD_STOREPASS`: the keystore's password, asked for on a terminal when it
  is unset.
- `TAWARA_UPLOAD_KEYPASS`: the key's password, when it is not the keystore's. It
  defaults to the keystore's.
- `TAWARA_ANDROID_APK=1`: also makes `Tawara-X.Y.Z+N.apk`, a universal APK for
  sideloading, signed with the same upload key. A dry run makes none.
- `TAWARA_ANDROID_DRY_RUN=1`: builds and checks everything, as step 3 says. It never
  signs.
- `TAWARA_ANDROID_OUT`: where the outputs go; `target/android-release` by default.
- `TAWARA_RELEASE_CACHE`: where the bundletool jar is kept; `~/.cache/tawara-release`
  by default.

### What it checks

The bundle's manifest must target API 36, carry the build number, not be debuggable,
turn backup off, name the backup rules and have `configChanges` `0xd800ffff`. Its
configuration must ask for 16 KB pages, and the notices must be under `base/assets/`.
The universal APK, when it is made, must be signed, aligned for 16 KB pages, carry
the arm64-v8a and x86_64 libraries, the notices under `assets/`, and the same
version, target and mask. A failed check stops the script.

### What the publisher supplies

None of these is in the repository or in CI.

- A Google Play Console account, with identity verification, and the app
  record for the application ID in `AndroidManifest.xml`.
- An upload keystore, made with `keytool` and kept with an offline backup.
  Its path, key alias and password are the values above. Play App Signing
  holds the app's signing key once the first upload enrols it.
- The app icon: a resource folder with the `mipmap-*` `ic_launcher` files,
  using Tawara's own mark rather than the coin mark. The listing also needs a
  512-pixel version.
- The Android SDK pieces named above, and a JDK 17 or later.
- In Play Console: confirmation of the target API rule; the closed test, if
  the account requires one; the Data safety form, the content rating, the
  target audience, the financial-features and cryptocurrency-policy
  declarations, and the listing and its screenshots.
- The device checks on an arm64 phone, and the emulator checks on the
  universal APK.
