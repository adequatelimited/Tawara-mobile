// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! The iOS app's executable. winit calls `UIApplicationMain` itself and
//! never returns, so this must be the process's `main`. Android starts
//! `android_main` in the library instead.

#[cfg(target_os = "ios")]
fn main() {
    tawara_mobile::ios::main();
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!(
        "{}: this is the iOS app's executable; run tawara-desktop on this system",
        tawara_app::DISPLAY_NAME
    );
    std::process::exit(2);
}
