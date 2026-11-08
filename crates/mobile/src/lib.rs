// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! Tawara for Android and iOS: the platform entry points and their glue.
//!
//! The desktop's `tawara-app` runs here through [`tawara_app::run_in`] with a
//! [`tawara_app::Host`] that provides the app-private directory, the clipboard,
//! the share sheet, Android's Back and the QR scanner camera. Each platform
//! also does the following:
//!
//! - **Android** (`src/android.rs`): `FLAG_SECURE` on the whole window; the
//!   store in `no_backup/`, which the manifest's backup rules also exclude; the
//!   lock when the system suspends the application; the QR scanner
//!   (`src/android/scan.rs`) using NDK Camera2, with frames decoded by
//!   `src/qr.rs`. `android.permission.CAMERA` is declared in the manifest and
//!   requested only when a scan starts. The scan stops when it ends, when the
//!   application moves to the background, when its window loses focus, and when
//!   the application exits.
//! - **iOS** (`src/ios.rs`): the store in Application Support, protected with
//!   `NSFileProtectionComplete` and excluded from backups; a cover over the
//!   window whenever the application is inactive, so the app switcher snapshot
//!   shows nothing; the lock on entering the background; the QR scanner
//!   (`src/ios/scan.rs`) using AVFoundation, requesting the camera only when a
//!   scan starts (`NSCameraUsageDescription` in `Info.plist.in`) and stopping it
//!   when the scan ends or the application enters the background; and the
//!   screenshot and screen-capture notifications used by the recovery phrase
//!   screen.
//!
//! There is no Java, Kotlin, Swift or Objective-C source: platform calls are
//! made from Rust through JNI on Android and the Objective-C runtime on iOS.

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "ios")]
pub mod ios;
// The Android QR decoder, also compiled for tests on every host.
#[cfg(any(target_os = "android", test))]
mod qr;

/// The Android entry point, called by android-activity on its own thread once
/// the activity is created. `AndroidApp` comes from the fork's winit through
/// `iced_winit`, the only copy of winit in the build.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: iced_winit::winit::platform::android::activity::AndroidApp) {
    android::main(app);
}
