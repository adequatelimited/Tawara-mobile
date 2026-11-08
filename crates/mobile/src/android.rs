// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! Android: the window's flags, the store's directory, the clipboard, the
//! share sheet, Back, the lock and the QR scanner (`android/scan.rs`), with
//! no Java code.
//!
//! The activity is `android.app.NativeActivity`. JNI reaches the platform's
//! classes through the `Application` that android-activity registers with
//! `ndk-context`. Diagnostics go to logcat under the tag `RustStdoutStderr`,
//! one `TAWARA` line per fact, never a secret. A pending Java exception from
//! the share sheet's calls is also described in the system log.

mod scan;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

// winit is reached through iced_winit so the build holds one copy of it.
use iced_winit::winit::platform::android::activity::{AndroidApp, WindowManagerFlags};
use jni::objects::{JObject, JString, JValue};
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str};
use tawara_app::host::ScanPreview;
use tawara_app::{Host, Leaving};

/// The activity, for Back, and for the scanner's permission and window.
static APP: OnceLock<AndroidApp> = OnceLock::new();

/// The lock started by the last move to the background, reported in the exit
/// line. `None` until the system has suspended the application.
static LEAVING: Mutex<Option<Leaving>> = Mutex::new(None);

/// Start the application in the activity android-activity has created.
pub fn main(app: AndroidApp) {
    // FLAG_SECURE keeps the window out of screenshots, recordings and the
    // recent-apps thumbnail. It does not stop every recording; Android
    // documents it as unreliable on some devices, notably API 30 and lower.
    // Set before the first frame.
    app.set_window_flags(WindowManagerFlags::SECURE, WindowManagerFlags::empty());
    let Some(private_dir) = private_dir(&app) else {
        // Logged directly, since `_exit` follows at once (see `log_direct`).
        log_direct("TAWARA stop: the app-private directory could not be made");
        // android-activity asks that `std::process::exit` not be called from
        // `android_main`.
        // SAFETY: `_exit` takes no pointer and does not return.
        unsafe { libc::_exit(1) }
    };
    eprintln!("TAWARA start: private directory {}", private_dir.display());
    let _ = APP.set(app.clone());
    iced_winit::set_android_app(app);
    iced_winit::on_lifecycle(lifecycle);
    let host = Host {
        private_dir,
        copy,
        share,
        back,
        // The third-party notices shown on the Licences page. release.sh puts
        // the same file in the bundle's assets/.
        notices: include_str!("../../../THIRD-PARTY-NOTICES.txt"),
        // Android reports no capture: FLAG_SECURE is its protection.
        captured: None,
        // Android offers no device unlock.
        device_check: None,
        keep_password: None,
        release_password: None,
        forget_password: None,
        // The application draws the preview from the frames the scan gives it.
        scan: Some(scan::start),
        cancel_scan: Some(scan::cancel),
        scan_preview: Some(ScanPreview::Application),
    };
    if let Err(error) = tawara_app::run_in(host) {
        log_direct(&format!("TAWARA stop: {error}"));
    }
    // Release the camera before the process ends, whatever ended the interface.
    scan::shutdown();
    // Checked without waiting: the interface completes the lock only in its
    // update, and winit has already ended the event loop by now.
    log_direct(&format!("TAWARA exit: {}", lock_state()));
    // The process ends here, so the next launch starts cold. `_exit`, not
    // `exit`: `exit` runs atexit handlers and static destructors while the
    // render threads still run, which aborts with SIGABRT on a destroyed
    // mutex.
    // SAFETY: `_exit` takes no pointer and does not return.
    unsafe { libc::_exit(0) }
}

/// `no_backup/`, beside `files/`: the system leaves it out of Auto Backup
/// and device transfer, and the manifest's rules exclude the rest. `files/`
/// is mode `0771`, which the library refuses for a store's parent, so
/// `no_backup/` is created with mode `0700` when missing.
fn private_dir(app: &AndroidApp) -> Option<PathBuf> {
    let files = app.internal_data_path()?;
    let dir = files.parent()?.join("no_backup");
    make_private(&dir).then_some(dir)
}

fn make_private(dir: &Path) -> bool {
    use std::os::unix::fs::DirBuilderExt;
    dir.is_dir()
        || std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .is_ok()
}

/// The system suspends the application, destroying its window, when it
/// leaves the screen. The wallet locks then.
fn lifecycle(event: iced_winit::Lifecycle) {
    if event == iced_winit::Lifecycle::Suspended {
        eprintln!("TAWARA lifecycle: suspended; locking");
        // Stop any scan first, so no camera runs in the background.
        scan::background();
        // Not waited for: Android does not suspend the process as soon as
        // this returns. The Leaving is kept for the exit line.
        let leaving = tawara_app::left_foreground();
        *LEAVING.lock().unwrap_or_else(PoisonError::into_inner) = Some(leaving);
    }
}

/// Whether the lock has finished, without waiting: `Leaving::wait` with a
/// zero duration is `true` once the interface has finished the move, or when
/// there was nothing to do.
fn lock_state() -> &'static str {
    let kept = LEAVING.lock().unwrap_or_else(PoisonError::into_inner);
    match kept.as_ref() {
        None => "no lock was started",
        Some(leaving) if leaving.wait(Duration::ZERO) => "lock done",
        Some(_) => "lock not done",
    }
}

/// Logs a line written just before the process ends. It goes to logcat
/// directly through `Log.i`, which hands it to logd before returning, because
/// android-activity's stderr forwarder may not read a line written just
/// before `_exit`. If JNI fails, the line goes to stderr with the reason.
fn log_direct(line: &str) {
    if let Err(error) = log_line(line) {
        eprintln!("{line} (not logged directly: {error:?})");
    }
}

/// Puts `text` on the clipboard through `ClipboardManager`, marked sensitive.
/// From Android 13 the system hides a sensitive copy from its clipboard
/// preview; earlier versions ignore the mark. Every copy is marked, including
/// signed spends, which can move funds while their reservation is open.
fn copy(text: &str) {
    if let Err(error) = clipboard_write(text) {
        eprintln!("TAWARA clipboard: {error:?}");
    }
}

/// Offers `text` to the system's share sheet through `Intent.createChooser`,
/// with `name` as the subject and the chooser's title. `Ok` means only that
/// the chooser was started; the application cannot learn what was done with
/// it.
///
/// The chooser is the system's own window, so `FLAG_SECURE` does not cover
/// it: it can show the text in a preview, appear in screenshots, and offer a
/// "Copy text" button that copies without the sensitive mark. This is
/// accepted, since the sheet opens only when the user chooses Save artifact
/// to hand these bytes to another app, and the only alternative is sharing a
/// file through a `FileProvider`, which needs Java code. The chooser is
/// translucent, so the wallet does not lock while it is up.
fn share(name: &str, text: &str) -> Result<(), String> {
    // `main` sets APP before `run_in`, so the activity is known here. A
    // missing activity is an error rather than a panic, so it shows as a
    // refusal, not a crash.
    let result = match APP.get() {
        Some(app) => share_text(app, name, text).map_err(|error| error.to_string()),
        None => Err("the activity is not known yet".to_owned()),
    };
    match &result {
        Ok(()) => eprintln!("TAWARA share: chooser started"),
        Err(error) => eprintln!("TAWARA share: {error}"),
    }
    result
}

/// Starts the chooser for `text` on the activity. The text travels in an
/// extra, so no file is written. A pending Java exception is described for
/// logcat and cleared, so the error returned stays one line.
fn share_text(app: &AndroidApp, name: &str, text: &str) -> jni::errors::Result<()> {
    // SAFETY: both pointers stay valid while `app` is alive (android-activity
    // documents `vm_as_ptr` and `activity_as_ptr` this way).
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity = app.activity_as_ptr() as jni::sys::jobject;
    vm.attach_current_thread(|env| {
        // SAFETY: the activity's own reference, alive while `app` is.
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity)? };
        let result = start_chooser(env, &activity, name, text);
        if result.is_err() && env.exception_check() {
            env.exception_describe();
            env.exception_clear();
        }
        result
    })
}

/// `ACTION_SEND` as `text/plain`, through `Intent.createChooser`, started on
/// `activity`.
fn start_chooser(
    env: &mut jni::Env,
    activity: &JObject,
    name: &str,
    text: &str,
) -> jni::errors::Result<()> {
    let action = env.new_string("android.intent.action.SEND")?;
    let mime = env.new_string("text/plain")?;
    let extra_text = env.new_string("android.intent.extra.TEXT")?;
    let extra_subject = env.new_string("android.intent.extra.SUBJECT")?;
    let value = env.new_string(text)?;
    let subject = env.new_string(name)?;
    let intent = env.new_object(
        jni_str!("android/content/Intent"),
        jni_sig!(() -> void),
        &[],
    )?;
    env.call_method(
        &intent,
        jni_str!("setAction"),
        jni_sig!((java.lang.String) -> android.content.Intent),
        &[JValue::Object(&action)],
    )?;
    env.call_method(
        &intent,
        jni_str!("setType"),
        jni_sig!((java.lang.String) -> android.content.Intent),
        &[JValue::Object(&mime)],
    )?;
    env.call_method(
        &intent,
        jni_str!("putExtra"),
        jni_sig!((java.lang.String, java.lang.String) -> android.content.Intent),
        &[JValue::Object(&extra_text), JValue::Object(&value)],
    )?;
    env.call_method(
        &intent,
        jni_str!("putExtra"),
        jni_sig!((java.lang.String, java.lang.String) -> android.content.Intent),
        &[JValue::Object(&extra_subject), JValue::Object(&subject)],
    )?;
    let chooser = env
        .call_static_method(
            jni_str!("android/content/Intent"),
            jni_str!("createChooser"),
            jni_sig!((android.content.Intent, java.lang.CharSequence) -> android.content.Intent),
            &[JValue::Object(&intent), JValue::Object(&subject)],
        )?
        .l()?;
    env.call_method(
        activity,
        jni_str!("startActivity"),
        jni_sig!((android.content.Intent) -> void),
        &[JValue::Object(&chooser)],
    )?;
    Ok(())
}

/// Back: moves the task to the background, as Android 12 and later do for a
/// launcher's root activity, keeping the activity and winit's event loop.
fn back() {
    let Some(app) = APP.get() else {
        return;
    };
    if let Err(error) = move_task_to_back(app) {
        eprintln!("TAWARA back: {error:?}");
    }
}

/// Runs `f` with the activity on the current thread, attached to the JVM for
/// the call; the scanner calls it from the camera's threads too. A pending
/// Java exception is described for logcat and cleared, so the caller gets
/// one error.
fn with_activity<T>(
    f: impl FnOnce(&mut jni::Env, &JObject) -> jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    let Some(app) = APP.get() else {
        return Err(jni::errors::Error::NullPtr("the activity is not known yet"));
    };
    // SAFETY: both pointers stay valid while `app` is alive (android-activity
    // documents `vm_as_ptr` and `activity_as_ptr` this way).
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity = app.activity_as_ptr() as jni::sys::jobject;
    vm.attach_current_thread(|env| {
        // SAFETY: the activity's own reference, alive while `app` is.
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity)? };
        let result = f(env, &activity);
        if result.is_err() && env.exception_check() {
            env.exception_describe();
            env.exception_clear();
        }
        result
    })
}

fn with_context<T>(
    f: impl FnOnce(&mut jni::Env, &JObject) -> jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    let ctx = ndk_context::android_context();
    // SAFETY: android-activity initialised ndk-context with a valid JavaVM
    // and a global reference to the Application before android_main.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let context = ctx.context() as jni::sys::jobject;
    vm.attach_current_thread(|env| {
        // SAFETY: a global reference that android-activity never deletes.
        let context = unsafe { env.as_cast_raw::<JObject>(&context)? };
        f(env, &context)
    })
}

/// Writes `line` to logcat under the tag the application's stderr lines carry.
fn log_line(line: &str) -> jni::errors::Result<()> {
    with_context(|env, _context| {
        let tag = env.new_string("RustStdoutStderr")?;
        let message = env.new_string(line)?;
        env.call_static_method(
            jni_str!("android/util/Log"),
            jni_str!("i"),
            jni_sig!((java.lang.String, java.lang.String) -> jint),
            &[JValue::Object(&tag), JValue::Object(&message)],
        )?;
        Ok(())
    })
}

fn clipboard_write(text: &str) -> jni::errors::Result<()> {
    with_context(|env, context| {
        let name = env.new_string("clipboard")?; // Context.CLIPBOARD_SERVICE
        let manager = env
            .call_method(
                context,
                jni_str!("getSystemService"),
                jni_sig!((java.lang.String) -> java.lang.Object),
                &[JValue::Object(&name)],
            )?
            .l()?;
        let label = env.new_string(tawara_app::DISPLAY_NAME)?;
        let value: JString = env.new_string(text)?;
        let clip = env
            .call_static_method(
                jni_str!("android/content/ClipData"),
                jni_str!("newPlainText"),
                jni_sig!((java.lang.CharSequence, java.lang.CharSequence) -> android.content.ClipData),
                &[JValue::Object(&label), JValue::Object(&value)],
            )?
            .l()?;
        // The mark goes on the clip's description before setPrimaryClip.
        // setExtras is API 24 (the minimum here is 26). The key is
        // ClipDescription.EXTRA_IS_SENSITIVE, written out because that field
        // does not exist below API 33, where the key is ignored. Any failure
        // returns before setPrimaryClip, so nothing reaches the clipboard
        // without the mark.
        let description = env
            .call_method(
                &clip,
                jni_str!("getDescription"),
                jni_sig!(() -> android.content.ClipDescription),
                &[],
            )?
            .l()?;
        let extras = env.new_object(
            jni_str!("android/os/PersistableBundle"),
            jni_sig!(() -> void),
            &[],
        )?;
        let key = env.new_string("android.content.extra.IS_SENSITIVE")?;
        env.call_method(
            &extras,
            jni_str!("putBoolean"),
            jni_sig!((java.lang.String, jboolean) -> void),
            &[JValue::Object(&key), JValue::Bool(true)],
        )?;
        env.call_method(
            &description,
            jni_str!("setExtras"),
            jni_sig!((android.os.PersistableBundle) -> void),
            &[JValue::Object(&extras)],
        )?;
        env.call_method(
            &manager,
            jni_str!("setPrimaryClip"),
            jni_sig!((android.content.ClipData) -> void),
            &[JValue::Object(&clip)],
        )?;
        Ok(())
    })
}

fn move_task_to_back(app: &AndroidApp) -> jni::errors::Result<()> {
    // SAFETY: both pointers stay valid while `app` is alive (android-activity
    // documents `vm_as_ptr` and `activity_as_ptr` this way).
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity = app.activity_as_ptr() as jni::sys::jobject;
    vm.attach_current_thread(|env| {
        // SAFETY: the activity's own reference, alive while `app` is.
        let activity = unsafe { env.as_cast_raw::<JObject>(&activity)? };
        env.call_method(
            &activity,
            jni_str!("moveTaskToBack"),
            jni_sig!((jboolean) -> jboolean),
            &[JValue::Bool(true)],
        )?;
        Ok(())
    })
}
