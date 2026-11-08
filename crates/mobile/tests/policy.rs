// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! Dependency and packaging rules for this repository, checked on the
//! committed manifests and lockfile, and on the features cargo resolves for
//! the Android targets.
//!
//! The application comes from Tawara-wallet, pinned by full commit hash, and
//! is never patched, replaced or overridden here. Every package from the iced
//! fork is a single copy at one commit, and no crate name comes from two
//! sources. Android's activity backend is winit's NativeActivity only. The
//! release profile that keeps key material scrubbable is restated, and the
//! mobile crate reaches the wallet only through `tawara-app`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const TAWARA_GIT: &str = "https://github.com/adequatelimited/Tawara-wallet";
const ICED_GIT: &str = "https://github.com/patricksmithlaravel/iced_mobile";

/// The two Android targets platform/android/tawara.sh builds.
/// `the_notices_describe_every_shipped_target` checks this list against the
/// script's TARGETS.
const ANDROID_TARGETS: [&str; 2] = ["aarch64-linux-android", "x86_64-linux-android"];

/// Every dependency the mobile crate may name: the application and the
/// platform glue. Everything else, notably the library and
/// `tawara-wallet-core`, is reached through `tawara-app` or not at all.
/// `winit` is excluded: it comes from the iced fork through `iced_winit`, and
/// a direct line would add a second copy or a second pin to keep in step.
/// `libc` provides `_exit` for the Android entry point. `ndk-sys` provides the
/// camera and image reader for Android's QR scanner, and `rqrr` decodes the
/// QR codes.
const MOBILE_DEPENDENCIES: [&str; 11] = [
    "tawara-app",
    "iced_winit",
    "jni",
    "ndk-context",
    "objc2",
    "block2",
    "objc2-foundation",
    "objc2-ui-kit",
    "libc",
    "ndk-sys",
    "rqrr",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root is two levels above crates/mobile")
}

fn read_toml(path: &Path) -> toml::Table {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse()
        .unwrap_or_else(|e| panic!("{} is not TOML: {e}", path.display()))
}

fn is_full_hash(rev: &str) -> bool {
    rev.len() == 40
        && rev
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn workspace_deps(manifest: &toml::Table) -> &toml::Table {
    manifest["workspace"]["dependencies"]
        .as_table()
        .expect("[workspace.dependencies] exists")
}

/// Asserts that `key` in [workspace.dependencies] is a git source pinned by a
/// full hash and selected by nothing else. Returns the hash.
fn pinned(manifest: &toml::Table, key: &str, git: &str) -> String {
    let dep = workspace_deps(manifest)
        .get(key)
        .and_then(|d| d.as_table())
        .unwrap_or_else(|| panic!("[workspace.dependencies] must name `{key}` as a table"));
    assert_eq!(
        dep.get("git").and_then(|v| v.as_str()),
        Some(git),
        "`{key}` must come from {git}"
    );
    let rev = dep.get("rev").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        is_full_hash(rev),
        "`{key}` must be pinned by a full 40-character lowercase commit hash, not {rev:?}"
    );
    let allowed = BTreeSet::from(["git", "rev", "default-features", "features"]);
    for k in dep.keys() {
        assert!(
            allowed.contains(k.as_str()),
            "`{key}` is chosen by its commit alone; it also names `{k}`"
        );
    }
    rev.to_owned()
}

/// Every package in Cargo.lock whose name `pick` accepts, with its source
/// (empty for a path package).
fn locked(lock: &toml::Table, pick: impl Fn(&str) -> bool) -> Vec<(&str, &str)> {
    lock["package"]
        .as_array()
        .expect("Cargo.lock lists packages")
        .iter()
        .filter_map(|p| p.as_table())
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            pick(name).then(|| (name, p.get("source").and_then(|v| v.as_str()).unwrap_or("")))
        })
        .collect()
}

/// The features cargo resolves for `package` when building for `target`.
/// Cargo.lock records no features, so `cargo tree` is used. `--locked` makes
/// it refuse a lockfile that does not match the manifests rather than
/// rewrite it.
fn resolved_features(target: &str, package: &str) -> BTreeSet<String> {
    let output = Command::new(env!("CARGO"))
        .current_dir(root())
        .args([
            "tree",
            "--locked",
            "--color",
            "never",
            "--target",
            target,
            "--invert",
            package,
            "--depth",
            "0",
            "--prefix",
            "none",
            "--format",
            "{p} | {f}",
        ])
        .output()
        .unwrap_or_else(|e| panic!("cannot run cargo tree: {e}"));
    assert!(
        output.status.success(),
        "cargo tree cannot resolve {package} for {target}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "the build for {target} holds {} copies of {package}; it must hold one",
        lines.len()
    );
    let line = lines[0];
    let (_, features) = line
        .split_once(" | ")
        .unwrap_or_else(|| panic!("cargo tree printed `{line}`, not a package and its features"));
    features
        .split(',')
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn nothing_is_patched_replaced_or_overridden() {
    let manifest = read_toml(&root().join("Cargo.toml"));
    for section in ["patch", "replace"] {
        assert!(
            !manifest.contains_key(section),
            "Cargo.toml has a [{section}] section"
        );
    }
    for (key, dep) in workspace_deps(&manifest) {
        assert!(
            dep.get("path").is_none(),
            "`{key}` is a path dependency; everything comes from a registry or a pinned commit"
        );
    }
    assert!(
        !root().join(".cargo").exists(),
        "a .cargo directory could override sources or patch crates"
    );
}

#[test]
fn the_application_comes_from_tawara_wallet_by_full_hash() {
    let manifest = read_toml(&root().join("Cargo.toml"));
    let rev = pinned(&manifest, "tawara-app", TAWARA_GIT);
    let lock = read_toml(&root().join("Cargo.lock"));
    let tawara = locked(&lock, |n| n.starts_with("tawara-") && n != "tawara-mobile");
    assert!(
        tawara.iter().any(|(n, _)| *n == "tawara-app")
            && tawara.iter().any(|(n, _)| *n == "tawara-wallet-core"),
        "Cargo.lock holds tawara-app and tawara-wallet-core: {tawara:?}"
    );
    let expected = format!("git+{TAWARA_GIT}?rev={rev}#{rev}");
    for (name, source) in tawara {
        assert_eq!(
            source, expected,
            "Cargo.lock resolved {name} from somewhere other than the pinned commit"
        );
    }
}

/// Every package Cargo.lock takes from the iced fork is at the commit pinned
/// for `iced_winit` and is the only package of its name, so the build holds
/// one copy of each. The fork also ships winit and dpi, so packages are
/// matched by the fork's URL, not by an `iced` name prefix.
#[test]
fn iced_is_one_copy_from_the_fork() {
    let manifest = read_toml(&root().join("Cargo.toml"));
    let rev = pinned(&manifest, "iced_winit", ICED_GIT);
    let lock = read_toml(&root().join("Cargo.lock"));
    let packages = locked(&lock, |_| true);
    let fork = format!("git+{ICED_GIT}");
    let from_fork: Vec<(&str, &str)> = packages
        .iter()
        .copied()
        .filter(|(_, source)| source.starts_with(&fork))
        .collect();
    for name in ["iced", "winit"] {
        assert!(
            from_fork.iter().any(|(n, _)| *n == name),
            "Cargo.lock holds no {name} from the fork; a check over nothing passes"
        );
    }
    let expected = format!("git+{ICED_GIT}?rev={rev}#{rev}");
    for (name, source) in from_fork {
        assert_eq!(
            source, expected,
            "Cargo.lock resolved {name} from the fork at another commit than the one \
             iced_winit pins"
        );
        let copies = packages.iter().filter(|(n, _)| *n == name).count();
        assert_eq!(
            copies, 1,
            "Cargo.lock holds {copies} packages named {name}; the build holds one copy of each"
        );
    }
}

/// No crate name comes from two sources in Cargo.lock, so a second copy of
/// any crate, from crates.io or another git commit, fails. Two versions of
/// one name from one source (as with `bitflags`) pass.
#[test]
fn no_crate_name_comes_from_two_sources() {
    let lock = read_toml(&root().join("Cargo.lock"));
    let mut sources: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (name, source) in locked(&lock, |_| true) {
        sources.entry(name).or_default().insert(source);
    }
    assert!(
        !sources.is_empty(),
        "Cargo.lock lists no packages; a check over nothing passes"
    );
    for (name, from) in &sources {
        assert_eq!(
            from.len(),
            1,
            "Cargo.lock takes {name} from {} sources ({from:?}); a crate name must come from one",
            from.len()
        );
    }
}

#[test]
fn the_release_profile_keeps_key_material_scrubbable() {
    let manifest = read_toml(&root().join("Cargo.toml"));
    let release = manifest
        .get("profile")
        .and_then(|p| p.get("release"))
        .and_then(|r| r.as_table())
        .expect("[profile.release] is restated");
    assert_eq!(
        release.get("panic").and_then(|v| v.as_str()),
        Some("unwind")
    );
    assert_eq!(
        release.get("overflow-checks").and_then(|v| v.as_bool()),
        Some(true)
    );
}

/// The dependency table kinds, each also possible under
/// `[target.'cfg(...)']`. A crate named in any of them is built, so the reach
/// rule covers all three.
const DEPENDENCY_KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// Whether the mobile crate may name `package` in a table of `kind`:
/// `MOBILE_DEPENDENCIES`, plus `toml` as a dev-dependency for this test.
fn allowed(kind: &str, package: &str) -> bool {
    MOBILE_DEPENDENCIES.contains(&package) || (kind == "dev-dependencies" && package == "toml")
}

/// Every dependency `manifest` names that the reach rule does not allow, as
/// `[section] key (package name)`. Both the key and the `package` value are
/// checked, so a renamed line such as `iced_winit = { package = "winit", ... }`
/// is caught.
fn strays_in_manifest(manifest: &toml::Table) -> Vec<String> {
    let mut tables: Vec<(String, &str, &toml::Table)> = Vec::new();
    for kind in DEPENDENCY_KINDS {
        if let Some(t) = manifest.get(kind).and_then(|d| d.as_table()) {
            tables.push((kind.to_owned(), kind, t));
        }
    }
    if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
        for (cfg, t) in targets {
            for kind in DEPENDENCY_KINDS {
                if let Some(d) = t.get(kind).and_then(|d| d.as_table()) {
                    tables.push((format!("target.{cfg}.{kind}"), kind, d));
                }
            }
        }
    }
    assert!(
        !tables.is_empty(),
        "the mobile crate names no dependency; a check over nothing passes"
    );
    let mut strays = Vec::new();
    for (section, kind, deps) in tables {
        for (key, dep) in deps {
            let package = dep
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or(key.as_str());
            if !allowed(kind, key) || !allowed(kind, package) {
                strays.push(format!("[{section}] {key} (package {package})"));
            }
        }
    }
    strays
}

/// Every package Cargo.lock records as a dependency of `tawara-mobile` that
/// the reach rule does not allow. The lock names packages, not keys, across
/// every table and target, so renamed lines show under their real package.
/// `toml` is allowed because the lock does not record its table; the
/// manifest walk restricts it to dev-dependencies.
fn strays_in_lock(lock: &toml::Table) -> Vec<String> {
    let mobile: Vec<&toml::Table> = lock["package"]
        .as_array()
        .expect("Cargo.lock lists packages")
        .iter()
        .filter_map(|p| p.as_table())
        .filter(|p| p.get("name").and_then(|v| v.as_str()) == Some("tawara-mobile"))
        .collect();
    assert_eq!(mobile.len(), 1, "Cargo.lock holds one tawara-mobile");
    let dependencies: Vec<&str> = mobile[0]
        .get("dependencies")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
        .filter_map(|d| d.as_str())
        .collect();
    assert!(
        !dependencies.is_empty(),
        "Cargo.lock records no dependency of tawara-mobile; a check over nothing passes"
    );
    dependencies
        .into_iter()
        .filter(|d| {
            let name = d.split(' ').next().unwrap_or(d);
            !allowed("dev-dependencies", name)
        })
        .map(str::to_owned)
        .collect()
}

/// The mobile crate names only the application and its platform glue, in any
/// table, under any name. A direct winit line fails even when the lock still
/// holds one copy.
#[test]
fn the_mobile_crate_reaches_the_wallet_only_through_the_app() {
    let manifest = read_toml(&root().join("crates/mobile/Cargo.toml"));
    let strays = strays_in_manifest(&manifest);
    assert!(
        strays.is_empty(),
        "crates/mobile names {strays:?}; it reaches the wallet only through tawara-app"
    );
    let lock = read_toml(&root().join("Cargo.lock"));
    let strays = strays_in_lock(&lock);
    assert!(
        strays.is_empty(),
        "Cargo.lock records {strays:?} as dependencies of tawara-mobile; it reaches the \
         wallet only through tawara-app"
    );
}

/// Lines that a check of [dependencies] keys alone would miss: winit under
/// [dev-dependencies] or [build-dependencies], and winit renamed to
/// `iced_winit`. Cargo builds each, so both the manifest walk and the lock
/// check must refuse them.
#[test]
fn the_reach_rule_refuses_winit_in_any_table_or_under_any_name() {
    let fixtures = [
        (
            "winit under [dev-dependencies]",
            r#"
                [dependencies]
                tawara-app.workspace = true
                [dev-dependencies]
                toml.workspace = true
                [dev-dependencies.winit]
                git = "https://github.com/patricksmithlaravel/iced_mobile"
                rev = "d4ab3a057db96162b087a27dd0752a3b15abfa6b"
            "#,
        ),
        (
            "winit renamed to iced_winit",
            r#"
                [dependencies]
                tawara-app.workspace = true
                [target.'cfg(target_os = "android")'.dependencies.iced_winit]
                package = "winit"
                git = "https://github.com/patricksmithlaravel/iced_mobile"
                rev = "d4ab3a057db96162b087a27dd0752a3b15abfa6b"
                features = ["android-native-activity"]
            "#,
        ),
        (
            "winit under a target's [build-dependencies]",
            r#"
                [dependencies]
                tawara-app.workspace = true
                [target.'cfg(target_os = "ios")'.build-dependencies]
                winit = "0.30.13"
            "#,
        ),
        (
            "toml outside [dev-dependencies]",
            r#"
                [dependencies]
                tawara-app.workspace = true
                toml.workspace = true
            "#,
        ),
    ];
    for (case, text) in fixtures {
        let manifest: toml::Table = text.parse().expect("the fixture is TOML");
        assert!(
            !strays_in_manifest(&manifest).is_empty(),
            "the manifest walk lets through {case}"
        );
    }
    let lock: toml::Table = r#"
        [[package]]
        name = "tawara-mobile"
        version = "0.1.0"
        dependencies = [
         "iced_winit",
         "tawara-app",
         "toml",
         "winit",
        ]
    "#
    .parse()
    .expect("the fixture is TOML");
    assert_eq!(
        strays_in_lock(&lock),
        ["winit"],
        "the lock check lets through a winit that tawara-mobile depends on directly"
    );
}

/// Android's activity backend is winit's NativeActivity only. A backend is
/// selected by a feature, so the check reads the `iced_winit` line and then
/// the features cargo resolves for each Android target, where a backend
/// enabled by another crate or by a default would appear.
#[test]
fn android_activity_has_the_native_activity_backend_only() {
    let manifest = read_toml(&root().join("Cargo.toml"));
    let chosen: Vec<&str> = workspace_deps(&manifest)
        .get("iced_winit")
        .and_then(|d| d.get("features"))
        .and_then(|f| f.as_array())
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str())
        .collect();
    assert!(
        chosen.contains(&"android-native-activity") && !chosen.contains(&"android-game-activity"),
        "the iced_winit line must choose android-native-activity and not android-game-activity; \
         it names {chosen:?}"
    );
    for target in ANDROID_TARGETS {
        let features = resolved_features(target, "android-activity");
        assert!(
            features.contains("native-activity") && !features.contains("game-activity"),
            "android-activity is built for {target} with features {features:?}; it must have \
             the native-activity backend and not the game-activity one"
        );
    }
}

/// The strings of a TOML array, in order. `what` names the array in a failure.
fn string_list(value: &toml::Value, what: &str) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("{what} is not an array"))
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("{what} holds a value that is not a string"))
                .to_owned()
        })
        .collect()
}

/// The licences the notices may name beyond deny.toml's allow list, and the
/// crate each is accepted for: the Mochimo licence for the library and
/// Tawara's crates, which deny.toml admits by name, and OFL-1.1 for the fonts
/// tawara-app bundles. tawara-app's code is also under the Mochimo licence,
/// which ci/notices.sh appends; about.toml lists it under OFL-1.1 so the font
/// licences are printed.
const NOTICES_EXCEPTIONS: [(&str, &str); 4] = [
    ("mochimo-crypto", "LicenseRef-Mochimo-Engine-1.0"),
    ("tawara-wallet-core", "LicenseRef-Mochimo-Engine-1.0"),
    ("tawara-mobile", "LicenseRef-Mochimo-Engine-1.0"),
    ("tawara-app", "OFL-1.1"),
];

/// The notices accept exactly the licences the policy gate admits, plus the
/// per-crate exceptions above. `about.toml`'s `accepted` must equal
/// `deny.toml`'s allow list: otherwise a licence could reach the notices
/// unchecked, or the generator could stop on a crate the gate has cleared.
/// Per-crate `accepted` lists must equal NOTICES_EXCEPTIONS, so a new
/// exception fails until it is added there.
#[test]
fn the_notices_accept_exactly_the_licences_the_policy_admits() {
    let deny = read_toml(&root().join("deny.toml"));
    let about = read_toml(&root().join("about.toml"));
    let mut allowed = string_list(&deny["licenses"]["allow"], "deny.toml's [licenses] allow");
    let mut accepted = string_list(&about["accepted"], "about.toml's accepted");
    allowed.sort();
    accepted.sort();
    assert_eq!(
        accepted, allowed,
        "about.toml accepts {accepted:?}; deny.toml admits {allowed:?}"
    );

    let mut per_crate: Vec<(String, Vec<String>)> = about
        .iter()
        .filter_map(|(name, value)| {
            let accepted = value.as_table()?.get("accepted")?;
            Some((
                name.clone(),
                string_list(accepted, &format!("about.toml's [{name}] accepted")),
            ))
        })
        .collect();
    per_crate.sort();
    let mut expected: Vec<(String, Vec<String>)> = NOTICES_EXCEPTIONS
        .iter()
        .map(|(name, licence)| ((*name).to_owned(), vec![(*licence).to_owned()]))
        .collect();
    expected.sort();
    assert_eq!(
        per_crate, expected,
        "about.toml's per-crate accepted lists must be exactly NOTICES_EXCEPTIONS; a new \
         exception needs its licence reviewed and approved first"
    );
}

/// The Android targets platform/android/tawara.sh builds, read from its
/// `TARGETS=(...)` line, in order.
fn android_script_targets() -> Vec<String> {
    let script = std::fs::read_to_string(root().join("platform/android/tawara.sh"))
        .expect("platform/android/tawara.sh is committed");
    let line = script
        .lines()
        .find_map(|line| line.strip_prefix("TARGETS=("))
        .expect("platform/android/tawara.sh has a TARGETS=(...) line");
    let list = line
        .strip_suffix(')')
        .expect("platform/android/tawara.sh's TARGETS=(...) fits on one line");
    list.split_whitespace().map(str::to_owned).collect()
}

/// The target platform/ios/tawara.sh's `device` step builds (the shipped iOS
/// build), read from its `cargo build ... --target` line.
fn ios_device_target() -> String {
    let script = std::fs::read_to_string(root().join("platform/ios/tawara.sh"))
        .expect("platform/ios/tawara.sh is committed");
    let body: Vec<&str> = script
        .lines()
        .skip_while(|line| *line != "device() {")
        .take_while(|line| *line != "}")
        .collect();
    let builds: Vec<String> = body
        .iter()
        .filter(|line| line.trim_start().starts_with("cargo build "))
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let at = words.iter().position(|w| *w == "--target")?;
            words.get(at + 1).map(|w| (*w).to_owned())
        })
        .collect();
    assert_eq!(
        builds.len(),
        1,
        "platform/ios/tawara.sh's device() should run one cargo build --target, and runs {builds:?}"
    );
    builds[0].clone()
}

/// The notices describe every shipped target and no other: the Android ABIs
/// platform/android/tawara.sh builds and the iOS device build of
/// platform/ios/tawara.sh, both read from the scripts. A missing target would
/// silently omit its crates from the notices, since the generator describes
/// only the targets it is given. ANDROID_TARGETS is checked against the
/// script too.
#[test]
fn the_notices_describe_every_shipped_target() {
    let android = android_script_targets();
    assert_eq!(
        android, ANDROID_TARGETS,
        "platform/android/tawara.sh builds {android:?}; ANDROID_TARGETS says {ANDROID_TARGETS:?}"
    );
    let mut shipped: Vec<String> = android;
    shipped.push(ios_device_target());
    shipped.sort();

    let about = read_toml(&root().join("about.toml"));
    let mut targets = string_list(&about["targets"], "about.toml's targets");
    targets.sort();
    assert_eq!(
        targets, shipped,
        "about.toml describes {targets:?}; the platform scripts ship {shipped:?}"
    );
}

/// The committed notices end with LICENSE.md byte for byte, as ci/notices.sh
/// appends it; cargo-about 0.9.2 does not write LicenseRef texts. The file
/// must name no build-machine path, hold no licence template placeholder, and
/// contain no carriage return.
#[test]
fn the_committed_notices_end_with_the_licence_and_name_no_build_path() {
    let notices = std::fs::read(root().join("THIRD-PARTY-NOTICES.txt"))
        .expect("THIRD-PARTY-NOTICES.txt is committed at the root");
    let licence =
        std::fs::read(root().join("LICENSE.md")).expect("LICENSE.md is committed at the root");
    assert!(
        notices.ends_with(&licence),
        "THIRD-PARTY-NOTICES.txt does not end with LICENSE.md, byte for byte"
    );
    let text = String::from_utf8_lossy(&notices);
    for marker in ["/Users/", "/home/", "/.cargo/"] {
        assert!(
            !text.contains(marker),
            "THIRD-PARTY-NOTICES.txt names a path of the machine that generated it ({marker})"
        );
    }
    let lower = text.to_lowercase();
    for marker in ["<year>", "<copyright holders>", "<owner>"] {
        assert!(
            !lower.contains(marker),
            "THIRD-PARTY-NOTICES.txt holds the licence template placeholder {marker}"
        );
    }
    assert!(
        !notices.contains(&b'\r'),
        "THIRD-PARTY-NOTICES.txt holds a carriage return; ci/notices.sh removes them"
    );
}

/// The crate versions that publish no MIT licence text in their package or
/// repository, and so get the standard text without the holder line
/// (configured in about.toml). A new crate or version must be checked before
/// it joins this list, since it may publish a text naming its holder.
const STANDARD_MIT_TEXT: [&str; 8] = [
    "block 0.1.6",
    "dispatch 0.2.0",
    "objc2 0.6.4",
    "objc2-core-foundation 0.3.2",
    "objc2-core-graphics 0.3.2",
    "objc2-encode 4.1.0",
    "objc2-foundation 0.3.2",
    "objc2-quartz-core 0.3.2",
];

/// The committed notices give the standard MIT text to exactly the crates in
/// STANDARD_MIT_TEXT.
#[test]
fn only_the_listed_crates_get_the_standard_mit_text() {
    let notices = std::fs::read_to_string(root().join("THIRD-PARTY-NOTICES.txt"))
        .expect("THIRD-PARTY-NOTICES.txt is committed at the root");
    let marker = "They publish none in their repository either";
    let entries: Vec<&str> = notices
        .split(
            "\n================================================================================\n",
        )
        .filter(|entry| entry.contains(marker))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "THIRD-PARTY-NOTICES.txt should have one standard MIT entry, and has {}",
        entries.len()
    );
    let mut named: Vec<String> = entries[0]
        .lines()
        .skip_while(|line| *line != "Used by:")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|line| {
            line.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    named.sort();
    let mut expected: Vec<String> = STANDARD_MIT_TEXT.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    assert_eq!(
        named, expected,
        "the crates given the standard MIT text are not STANDARD_MIT_TEXT; check that each new one \
         publishes no licence text of its own, in its package or its repository, first"
    );
}

/// Runs platform/build-number.sh in `dir` with `args`, and TAWARA_BUILD_NUMBER
/// set to `store` if given. Returns success and stdout without the final
/// newline.
fn build_number(dir: &Path, store: Option<&str>, args: &[&str]) -> (bool, String) {
    let mut command = Command::new("bash");
    command
        .arg(dir.join("platform/build-number.sh"))
        .args(args)
        .env_remove("TAWARA_BUILD_NUMBER");
    if let Some(number) = store {
        command.env("TAWARA_BUILD_NUMBER", number);
    }
    let output = command
        .output()
        .expect("bash runs platform/build-number.sh");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_owned(),
    )
}

/// The build number rises by one for every upload to either store, and a
/// store build uses only the committed counter. platform/build-number.sh
/// enforces both, and both platform scripts take their number from it (iOS
/// CFBundleVersion, Android versionCode). Stores refuse a number they have
/// seen, so a decrease, repeat, skip or non-integer must never reach a build.
#[test]
fn the_build_number_rises_by_one_for_every_upload() {
    let root = root();

    // NEW follows OLD, both plain integers, and NEW is within Google Play's
    // versionCode limit.
    for (new, old) in [("1", "0"), ("7", "6"), ("2100000000", "2099999999")] {
        let (ok, _) = build_number(&root, None, &["check", new, old]);
        assert!(
            ok,
            "check {new} {old}: {new} follows {old}, and must be admitted"
        );
    }
    // Repeated, lowered, skipped, not a plain integer, or out of range.
    for (new, old) in [
        ("6", "6"),
        ("5", "6"),
        ("8", "6"),
        ("0", "0"),
        ("0", "-1"),
        ("07", "6"),
        ("7.0", "6"),
        ("+7", "6"),
        ("-7", "6"),
        ("7a", "6"),
        (" 7", "6"),
        ("", "6"),
        ("2100000001", "2100000000"),
    ] {
        let (ok, _) = build_number(&root, None, &["check", new, old]);
        assert!(!ok, "check '{new}' '{old}' must be refused");
    }

    // The committed counter is valid, and a build without a store number uses
    // the development number, 1.
    let (ok, _) = build_number(&root, None, &["counter"]);
    assert!(ok, "platform/build-number must hold one plain integer");
    let (ok, number) = build_number(&root, None, &["number"]);
    assert!(
        ok && number == "1",
        "a build with no TAWARA_BUILD_NUMBER takes the development number 1, not {number:?}"
    );

    // `bump` raises the counter by exactly one. Work on a copy so the
    // committed counter is untouched.
    let dir = std::env::temp_dir().join(format!("tawara-build-number-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("platform")).expect("a scratch directory can be made");
    std::fs::copy(
        root.join("platform/build-number.sh"),
        dir.join("platform/build-number.sh"),
    )
    .expect("the script can be copied");
    let file = dir.join("platform/build-number");
    std::fs::write(&file, "6\n").expect("the counter can be written");
    for refused in ["5", "6", "8", "07"] {
        let (ok, _) = build_number(&dir, None, &["bump", refused]);
        assert!(!ok, "bump {refused} must be refused while the counter is 6");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the counter can be read"),
            "6\n",
            "a refused bump leaves the counter as it was"
        );
    }
    let (ok, _) = build_number(&dir, None, &["bump", "7"]);
    assert!(ok, "bump 7 follows the counter 6");
    assert_eq!(
        std::fs::read_to_string(&file).expect("the counter can be read"),
        "7\n"
    );

    // A store build accepts only the committed counter.
    let (ok, number) = build_number(&dir, Some("7"), &["number"]);
    assert!(
        ok && number == "7",
        "a store build of 7 takes 7 while the counter is 7, not {number:?}"
    );
    for other in ["6", "8", "0", "07", "x"] {
        let (ok, _) = build_number(&dir, Some(other), &["number"]);
        assert!(
            !ok,
            "a store build of {other} must be refused while the counter is 7"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);

    // Both platforms take the number from the counter, not a literal.
    let plist = std::fs::read_to_string(root.join("platform/ios/Info.plist.in"))
        .expect("platform/ios/Info.plist.in is committed");
    let version = plist
        .split("<key>CFBundleVersion</key>")
        .nth(1)
        .and_then(|rest| rest.lines().find(|line| line.contains("<string>")))
        .unwrap_or("");
    assert!(
        version.contains("<string>@BUILD@</string>"),
        "CFBundleVersion must be @BUILD@, not {version:?}"
    );
    let ios = std::fs::read_to_string(root.join("platform/ios/tawara.sh"))
        .expect("platform/ios/tawara.sh is committed");
    assert!(
        ios.contains(r#"s/@BUILD@/$build/"#),
        "platform/ios/tawara.sh must fill @BUILD@ with the build number"
    );
    let android = std::fs::read_to_string(root.join("platform/android/tawara.sh"))
        .expect("platform/android/tawara.sh is committed");
    assert!(
        android.contains(r#"--version-code "$build""#),
        "platform/android/tawara.sh must link the build number as the versionCode"
    );
    for (name, script) in [("iOS", &ios), ("Android", &android)] {
        assert!(
            script.contains(r#". "$ROOT/platform/build-number.sh""#)
                && script.contains("bn_number"),
            "the {name} script must take its build number from platform/build-number.sh"
        );
    }
}

/// Every `<activity>` element of the committed Android manifest, from its
/// opening `<activity` to the `>` that closes the start tag.
fn activity_tags(manifest: &str) -> Vec<&str> {
    manifest
        .match_indices("<activity")
        .map(|(start, _)| {
            let rest = &manifest[start..];
            &rest[..rest.find('>').map_or(rest.len(), |end| end + 1)]
        })
        .collect()
}

/// Tawara's task has an empty affinity, so no other application's activity
/// can join it or be placed in it, and no activity moves between tasks by
/// affinity.
#[test]
fn the_android_task_has_no_affinity_to_share() {
    let manifest = std::fs::read_to_string(root().join("platform/android/AndroidManifest.xml"))
        .expect("platform/android/AndroidManifest.xml is committed");
    let activities = activity_tags(&manifest);
    assert!(!activities.is_empty(), "the manifest declares an activity");
    for tag in &activities {
        assert!(
            tag.contains(r#"android:taskAffinity="""#),
            "every activity must set android:taskAffinity to \"\": {tag}"
        );
        assert_eq!(
            tag.matches("android:taskAffinity").count(),
            1,
            "an activity sets android:taskAffinity once: {tag}"
        );
    }
    assert!(
        !manifest.contains(r#"android:allowTaskReparenting="true""#),
        "no element may allow task reparenting"
    );
}
