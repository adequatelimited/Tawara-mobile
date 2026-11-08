# iOS platform glue

Tawara on iOS is `crates/mobile`'s `tawara` binary as the app bundle's
executable, with an `Info.plist` made from `Info.plist.in`: no Xcode
project, storyboard, Swift or Objective-C. The bundle identifier is
`CFBundleIdentifier` in `Info.plist.in`.

- `Info.plist.in`: the bundle's keys, including the scene manifest (without
  it UIKit stops an app linked with the iOS 27 SDK at launch), the status-bar
  keys that keep the clock and icons light on the dark screens, and the usage
  texts for Face ID (`NSFaceIDUsageDescription`, used by the device unlock)
  and the camera (`NSCameraUsageDescription`, used by the QR scanner).
  `tawara.sh` fills in the platform, the minimum system version, the version
  and the build number (`CFBundleVersion`, from `platform/build-number.sh`).
- `tawara.sh`: `device` (built and linked for a phone; installing it needs
  signing) and `simulator` (bundled, signed ad hoc, and run on a simulator
  with the checks CI runs, `.github/workflows/mobile.yml`). These checks do
  not exercise the device unlock, since `simctl` has no biometric command, or
  the scanner, since the simulator has no camera (a scan there is refused at
  once with `TAWARA scan: no camera`).
- Frameworks: the device unlock links `Security` and `LocalAuthentication`
  with `#[link]` in `crates/mobile/src/ios/keychain.rs` and calls them
  through `objc2` and `objc2-foundation`. The QR scanner links `AVFoundation`
  the same way in `crates/mobile/src/ios/scan.rs`, with hand-written `objc2`
  bindings.

What the Rust side does (`crates/mobile/src/ios.rs`):

- The store lives under `Library/Application Support/Tawara`, mode `0700`,
  protected with `NSFileProtectionComplete` and excluded from iCloud and
  device backups with `NSURLIsExcludedFromBackupKey`.
- Copies to the pasteboard are local to the device and expire after two
  minutes.
- The share sheet is a `UIActivityViewController` without its own Copy, on
  the top controller of the key window.
- A cover hides the key window whenever the application is not active, so
  the app switcher's snapshot shows nothing. If UIKit has no key window, a
  log line says so, which fails the simulator check.
- The wallet locks when the application enters the background.
- Screenshot and screen-capture reports, shown on the recovery phrase's
  screen, come from `UIApplicationUserDidTakeScreenshotNotification` and
  `UIScreenCapturedDidChangeNotification`.
- Placing winit's window in the window scene UIKit connects is done by the
  iced fork, since winit 0.30 has no scene support; the app supplies the
  scene manifest.

The QR scanner (`crates/mobile/src/ios/scan.rs`) fills the `Host` fields
that start and stop a scan, and the one that says the shell draws the
camera's preview. A scan is a full-screen, portrait-only modal over the key
window, with the preview and a Cancel button. The capture session starts and
stops on its own serial queue, and readings arrive on another. The first
reading that holds exactly one distinct text is returned to the application
once, unchecked; the application trims it, bounds it and validates it as a
destination. A reading of two different texts is ignored. A scan also stops
when the user cancels, when the application asks, and when the application
goes to the background. The camera is requested only when a scan starts.
Android's scanner is described in `platform/android/README.md`.

The device unlock (`crates/mobile/src/ios/keychain.rs`) fills the `Host`
fields that offer the check and keep the store's password for it; Android
leaves them empty. The check is the device's own, through `LAContext`: none
is offered when the device has no passcode; otherwise it is Face ID or Touch
ID when biometry is usable, and the passcode when it is not. The password is
one generic-password Keychain item under the service `<bundle id>.unlock`,
labelled with the store's identifier (32 random, non-secret bytes kept in its
folder, as lower-case hex). Its access control requires the device's check
(`kSecAccessControlUserPresence`), and the item is readable only while the
device has a passcode, on this device only. Keeping a password replaces the
previous one, and an item whose identifier belongs to another store is
deleted, never used. A release runs the check on its own thread with the
reason the application passes, and answers once. The copy made for the
Keychain is wiped once the Keychain has taken it, and a password read back
moves at once into the wallet's zeroizing buffer. Diagnostics on stderr are
`TAWARA unlock` lines, one per fact; none carries a password or its bytes.

On the simulator the Keychain refuses an add from a binary signed ad hoc
without entitlements, as `tawara.sh` signs the bundle: the add fails with
-34018 (a required entitlement is not present) and no password is kept. A
signed build carries the needed entitlement. Face ID is enrolled in the
Simulator app's Features menu.

The signed release build adds the
`com.apple.developer.default-data-protection` entitlement, so every file the
application creates is protected from the start.

## The App Store upload

`release.sh` makes `Tawara-X.Y.Z+N.ipa`. It runs on a Mac, from the commit
tagged for the upload; CI never runs it.

It builds the `aarch64-apple-ios` release binary and makes `Tawara.app` from
`Info.plist.in`, with the `DT` keys of the Xcode in use, the build number as
`CFBundleVersion`, the icon compiled into `Assets.car` with `actool`,
`PrivacyInfo.xcprivacy`, and `THIRD-PARTY-NOTICES.txt` and `LICENSE.md` at the
bundle's root. With the signing variables set, it writes the entitlements,
embeds the App Store profile, signs the bundle, reads the entitlements back,
zips `Payload` with `ditto` and checks the archive. It prints the `altool`
upload commands and runs none of them.

### Release steps

Steps 2 to 5 change and tag the repository, so the release is built from a
commit that records its own version and build number.

1. Gather the items listed under "What the publisher supplies", and install
   Xcode 26.6 at `/Applications/Xcode_26.6.app` beside Xcode 27.
2. Set the version. `version` under `[workspace.package]` in `Cargo.toml` is
   the release's `X.Y.Z`, and the `tawara-app` `rev` is the Tawara-wallet
   release commit, by its full hash. The release refuses unless `Cargo.lock`
   gives `tawara-app` the same version.
3. Raise the build number. `platform/build-number.sh next` prints N, and
   `platform/build-number.sh bump N` records it in `platform/build-number`.
4. Regenerate the notices with `bash ci/notices.sh`.
5. Commit steps 2 to 4 as the release commit, then tag it `vX.Y.Z+N` with an
   annotated tag whose message names the library's `rev`, the fork's `rev`
   and the wallet commit. Tags are never moved.
6. Check out the tag. The release refuses a tree that differs from its commit.
7. Optional: a dry run, with `TAWARA_IOS_DRY_RUN=1` and
   `TAWARA_EXPORT_COMPLIANCE` set. It builds and checks everything, and signs
   and zips nothing.
8. With the variables below set, `TAWARA_BUILD_NUMBER=N` among them, run
   `platform/ios/release.sh`. With the signing variables set, it prints the
   path and SHA-256 of `Tawara-X.Y.Z+N.ipa` and the commands that validate
   and upload it. When a signing variable is unset, it names it, makes no
   `.ipa` and exits 0, so read the last line.
9. Validate and upload the `.ipa` by hand, with those commands or with
   Transporter, then run the TestFlight build and the device checks.

### Environment

None of these values is written into a file the repository keeps.

- `TAWARA_EXPORT_COMPLIANCE`, required in every mode: `true` or `false`, the
  answer on encryption. It becomes `ITSAppUsesNonExemptEncryption`, which
  `Info.plist.in` leaves out on purpose. There is no default.
- `TAWARA_BUILD_NUMBER`, required for a release: the committed counter. Run
  `platform/build-number.sh bump N`, commit it, then set this to N. A dry run
  uses the development number, 1.
- `TAWARA_IOS_ICON_DIR`, required for a release: the folder that holds
  `Assets.xcassets`, with an `AppIcon` set. A dry run without it leaves the
  icon out, with a warning.
- `TAWARA_TEAM_ID`, `TAWARA_IOS_PROFILE`, `TAWARA_IOS_SIGN_ID`: the team, the App
  Store profile and the Apple Distribution identity. If any is unset, the script
  stops before signing, names what is unset, makes no `.ipa` and prints no upload
  command. It exits 0 there, so read the last line.
- `TAWARA_IOS_DRY_RUN=1`: build and check, then stop before signing. A dry run is
  never uploaded, so an SDK other than 26.x is a warning in a dry run and a refusal
  in a release.
- `DEVELOPER_DIR`: when it is unset and `/Applications/Xcode_26.6.app` exists, that
  Xcode is used. Otherwise the active one is, and a release needs both the iPhoneOS
  SDK and the SDK the binary is linked with (`vtool -show-build`) to be 26.x.
- `TAWARA_IOS_OUT`: where the bundle and the `.ipa` go; `target/ios-release` by
  default, which git ignores.

Before anything is signed, a release refuses: a missing `TAWARA_BUILD_NUMBER`
or icon; tracked files that differ from the commit; a build number that is not
the committed counter (`bn_number`); a `tawara-mobile` version that differs
from the `tawara-app` version in `Cargo.lock`; a binary that is not an iOS
device binary with minos 16.0; and a non-26.x SDK. The profile must be an App
Store profile for this app, with `get-task-allow` false and no device list.
After signing, the entitlements codesign reads back must equal the file the
bundle was signed with. The archive must hold `Payload/Tawara.app/` and no
AppleDouble `._` entry.

Entitlements (`write_entitlements`): `application-identifier` and
`com.apple.developer.team-identifier` from `TAWARA_TEAM_ID`;
`com.apple.developer.default-data-protection` as `NSFileProtectionComplete`;
`get-task-allow` false. There is no `keychain-access-groups`: the application
identifier is the default Keychain access group, so the unlock item belongs
to this app alone. The -34018 failure above comes from a bundle with no
application identifier, which an ad hoc signed bundle lacks.

### Privacy manifest

`PrivacyInfo.xcprivacy` is derived from `nm -u` on the release binary:
`C617.1` for FileTimestamp, because the binary imports `stat`, `lstat`,
`fstat` and `fstatat`, which read metadata of the store's files in the app's
container; `35F9.1` for SystemBootTime, because it imports
`mach_absolute_time`, which Rust's `Instant` uses. It imports no disk-space
call, `NSUserDefaults` or active-keyboard API, so none is declared.
`clock_gettime` is imported but has no required-reason category.
`NSPrivacyTracking` is false and `NSPrivacyCollectedDataTypes` is empty, for
the reasons recorded in the file. Re-check the imports on the build that is
submitted. The App Store privacy answers must agree with the file.

### What the publisher supplies

None of these is in the repository or in CI.

- Enrolment in the Apple Developer Program as an organisation (App Review
  guideline 3.1.5(i)).
- The App ID matching `CFBundleIdentifier`, with Data Protection set to
  Complete Protection, and the App Store Connect app record for it.
- The team identifier, 10 characters: `TAWARA_TEAM_ID`.
- An Apple Distribution certificate in the keychain of the signing Mac. Its identity
  name, as `security find-identity` lists it: `TAWARA_IOS_SIGN_ID`.
- An App Store provisioning profile for that App ID, as a `.mobileprovision` file:
  `TAWARA_IOS_PROFILE`. The script refuses a profile for another app, a profile that
  lists devices, and one that does not say `get-task-allow` is false.
- An App Store Connect API key, with its key ID and issuer ID, for `altool` or
  Transporter. The script never reads the key, and the printed commands take the two
  IDs.
- The app icon: a folder holding `Assets.xcassets` with an `AppIcon` set,
  opaque at 1024 pixels, using Tawara's own mark rather than the coin mark:
  `TAWARA_IOS_ICON_DIR`.
- The export-compliance answer: `TAWARA_EXPORT_COMPLIANCE`. `true` means the
  app uses encryption beyond the exempt kinds; `false` means it does not.
- The App Store privacy answers, which must agree with
  `PrivacyInfo.xcprivacy`, with the reason codes confirmed against Apple's
  list for the submitted build.
- Xcode 26.6 beside Xcode 27.
- The upload, the TestFlight build and the device checks on the signed build.
