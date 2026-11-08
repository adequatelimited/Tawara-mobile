# Tawara for Android and iOS

The mobile apps of [Tawara](https://github.com/adequatelimited/Tawara-wallet),
a graphical wallet for Mochimo, written in Rust with the
[iced](https://iced.rs) toolkit.

This repository holds only the mobile parts: the Android and iOS entry
points, their platform glue and their platform files. The application
itself (every screen, the wallet worker and the wallet library) is
`tawara-app` from Tawara-wallet, the same crate the desktop runs. It is a
dependency pinned by full commit hash and never patched, vendored or copied
here: changes to screens, wallet logic or user-facing text are made in
Tawara-wallet and taken here by moving `rev` in `Cargo.toml`.
`crates/mobile/tests/policy.rs` enforces this.

Tawara is maintained, owned and published by **Adequate Systems HK Limited**.

- Original Author: Kami Sama
- Maintainer, owner and publisher: Adequate Systems HK Limited
- Contact: support@adequatelimited.com

## Layout

```
crates/
  mobile/        the tawara-mobile crate: the Android library and the iOS binary
    tests/       the policy test
ci/
  notices.sh     writes THIRD-PARTY-NOTICES.txt, which CI compares with the committed file
platform/
  android/       manifest, backup rules, build, check and release scripts
  ios/           Info.plist, privacy manifest, build, check and release scripts
  build-number   the store build counter, and build-number.sh, which raises it
about.toml, THIRD-PARTY-NOTICES.hbs, THIRD-PARTY-NOTICES.txt   the notices (below)
.github/workflows/mobile.yml   the checks, the emulator and the simulators
```

## Building

The compiler is pinned in `rust-toolchain.toml`, as in Tawara-wallet.

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked          # the policy test
cargo deny check                         # needs cargo-deny 0.20.2 and the network
```

Android needs the Android SDK and NDK (`ANDROID_HOME`):

```
platform/android/tawara.sh build      # libtawara_mobile.so, arm64-v8a and x86_64
platform/android/tawara.sh apk        # the APK, signed with a throwaway debug key
platform/android/tawara.sh emulator   # boot an emulator, install, run the checks
```

`platform/android/release.sh` builds the Google Play bundle. It needs the
pinned Android SDK components and the upload key, and CI never runs it
(`platform/android/README.md`).

iOS needs a Mac with Xcode:

```
platform/ios/tawara.sh device      # build and link for a phone
platform/ios/tawara.sh simulator   # bundle, boot a simulator, run the checks
```

`platform/ios/release.sh` builds the App Store package. It needs Xcode 26.x and
the publisher's signing identities, and CI never runs it
(`platform/ios/README.md`).

The steps of each store build, the environment variables it reads and the
inputs it needs (accounts, signing identities and profile, upload key, icons and
the export compliance answer) are in the two platform READMEs. CI signs and
uploads nothing.

The build number is `CFBundleVersion` on iOS and `versionCode` on Android.
Each upload to either store takes the next one. `platform/build-number` holds
the last number uploaded, and `platform/build-number.sh bump N` raises it,
only to the number after it. Debug and CI builds use the development number,
1. A store build sets `TAWARA_BUILD_NUMBER` to the committed counter, and both
platform scripts refuse any other value.

CI runs all of these (`.github/workflows/mobile.yml`). Each push to `main`
also builds the signed Android App Bundle and APK, a tag `vX.Y.Z` publishes
them as a GitHub release, and the unsigned iOS bundle is kept as a workflow
artifact. The upload key reaches CI through the repository secrets
`TAWARA_UPLOAD_KEYSTORE_B64`, `TAWARA_UPLOAD_KEY_ALIAS` and
`TAWARA_UPLOAD_STOREPASS`.

## Third-party notices

`THIRD-PARTY-NOTICES.txt` lists the crates the Android and iOS builds include,
each with its licence text, and ends with the Mochimo licence from
`LICENSE.md`. It is generated with cargo-about 0.9.2, not written by hand:

```
cargo install --locked --features cli cargo-about --version 0.9.2
bash ci/notices.sh            # write THIRD-PARTY-NOTICES.txt
bash ci/notices.sh --check    # what CI runs: fail, with a diff, if it differs
```

`about.toml` lists the accepted licences, the targets described, and the
licence files that name each crate's copyright holders where cargo-about would
not find them; some are fetched from the crate's repository, so generating the
file needs the network. `THIRD-PARTY-NOTICES.hbs` is the layout.
`ci/notices.sh` removes carriage returns and the standard MIT text's template
holder line, and appends `LICENSE.md`, because cargo-about 0.9.2 does not write
that licence's text. CI regenerates the file and fails on any difference, and
`crates/mobile/tests/policy.rs` checks `about.toml` against the licences
`deny.toml` admits and the targets the platform scripts build.

The notices ship in both store packages: `platform/ios/release.sh` places the
file and `LICENSE.md` at the root of `Tawara.app`, and
`platform/android/release.sh` places them under the bundle's `assets/`. The
development builds from `platform/ios/tawara.sh` and
`platform/android/tawara.sh apk` do not include them.

## Licence

Copyright Adequate Systems HK Limited.

Licensed under the Mochimo Cryptocurrency Engine License Agreement, version
1.0, in [LICENSE.md](LICENSE.md), the same licence as Tawara-wallet and the
wallet library.
