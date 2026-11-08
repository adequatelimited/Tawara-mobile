// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! iOS: the store's directory, its protection and backup exclusion, the
//! pasteboard, the share sheet, the QR scanner, the app-switcher cover, the
//! lock, and the screenshot and capture reports used by the recovery phrase's
//! screen, through the Objective-C runtime with no Swift or Objective-C
//! source.
//!
//! Everything here runs on the main thread: [`main`] before winit's
//! `UIApplicationMain`, and the notification blocks on the main queue. The
//! exceptions are the device unlock's check, which waits for the user on its
//! own thread (`keychain.rs`), and the scanner's session and readings, on
//! their own queues (`scan.rs`). Diagnostics on stderr are `TAWARA` lines,
//! one per fact, never a secret.

mod keychain;
mod scan;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{
    MainThreadMarker, NSArray, NSCopying, NSDate, NSDictionary, NSFileManager,
    NSFileProtectionComplete, NSFileProtectionKey, NSHomeDirectory, NSNotification,
    NSNotificationCenter, NSNotificationName, NSNumber, NSOperationQueue, NSString, NSURL,
    NSURLIsExcludedFromBackupKey,
};
use objc2_ui_kit::{
    UIActivityTypeCopyToPasteboard, UIActivityViewController, UIApplication,
    UIApplicationDidBecomeActiveNotification, UIApplicationDidEnterBackgroundNotification,
    UIApplicationUserDidTakeScreenshotNotification, UIApplicationWillResignActiveNotification,
    UIBackgroundTaskInvalid, UIColor, UIPasteboard, UIPasteboardOptionExpirationDate,
    UIPasteboardOptionLocalOnly, UIScreen, UIScreenCapturedDidChangeNotification, UIView,
    UIViewAutoresizing, UIWindow,
};
use tawara_app::Host;
use tawara_app::host::ScanPreview;

thread_local! {
    /// The cover over the window while the application is not active.
    static COVER: RefCell<Option<Retained<UIView>>> = const { RefCell::new(None) };
    /// The share sheet requested from UIKit and not yet shown: its number and
    /// when it was requested.
    static OPENING: Cell<Option<(u64, Instant)>> = const { Cell::new(None) };
    /// The number of the last share sheet requested.
    static SHEETS: Cell<u64> = const { Cell::new(0) };
}

/// How long a share sheet may take to open before it is assumed refused by
/// UIKit. Sheets normally appear within 0.6 to 2.4 s; five seconds allows for
/// slower phones without blocking a retry for long.
const OPENING_LIMIT: Duration = Duration::from_secs(5);

/// Starts the application. Never returns: winit's `UIApplicationMain` ends
/// the process.
pub fn main() {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("TAWARA stop: not started on the main thread");
        std::process::exit(1);
    };
    let Some(private_dir) = private_dir() else {
        eprintln!("TAWARA stop: the app-private directory could not be made");
        std::process::exit(1);
    };
    eprintln!("TAWARA start: private directory {}", private_dir.display());
    observe(mtm);
    let host = Host {
        private_dir,
        copy,
        share,
        back: || {},
        // The third-party notices shown on the Licences page. release.sh puts
        // the same file at the root of Tawara.app.
        notices: include_str!("../../../THIRD-PARTY-NOTICES.txt"),
        captured: Some(is_captured),
        device_check: Some(keychain::device_check),
        keep_password: Some(keychain::keep),
        release_password: Some(keychain::release),
        forget_password: Some(keychain::forget),
        // The scanner presents its own UIKit screen with the camera preview.
        scan: Some(scan::start),
        cancel_scan: Some(scan::cancel),
        scan_preview: Some(ScanPreview::Shell),
    };
    if let Err(error) = tawara_app::run_in(host) {
        eprintln!("TAWARA stop: {error}");
        std::process::exit(1);
    }
}

/// `Library/Application Support/Tawara` in the app's container, mode `0700`,
/// protected with `NSFileProtectionComplete` and excluded from iCloud and
/// device backups. The store goes in its own folder inside it. Protection is
/// reapplied to everything there at each start.
fn private_dir() -> Option<PathBuf> {
    // SAFETY: NSHomeDirectory has no preconditions and returns a valid,
    // autoreleased NSString.
    let home = PathBuf::from(unsafe { NSHomeDirectory().as_ref() }.to_string());
    let dir = home.join("Library/Application Support/Tawara");
    {
        use std::os::unix::fs::DirBuilderExt;
        if !dir.is_dir() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .ok()?;
        }
    }
    let protected = protect(&dir);
    let excluded = exclude_from_backup(&dir);
    eprintln!("TAWARA store: protected={protected} excluded_from_backup={excluded}");
    (protected && excluded).then_some(dir)
}

/// Sets `NSFileProtectionComplete` on `path` and everything below it.
fn protect(path: &Path) -> bool {
    let mut ok = set_protection(path);
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            ok &= if is_dir {
                protect(&child)
            } else {
                set_protection(&child)
            };
        }
    }
    ok
}

fn set_protection(path: &Path) -> bool {
    // SAFETY: the keys and values are Foundation's own constants, and the
    // file manager is used as documented, with a valid path.
    unsafe {
        let value: Retained<AnyObject> =
            Retained::into_super(Retained::into_super(NSFileProtectionComplete.copy()));
        let attributes = NSDictionary::from_vec(&[NSFileProtectionKey], vec![value]);
        NSFileManager::defaultManager()
            .setAttributes_ofItemAtPath_error(&attributes, &ns_path(path))
            .map_err(|e| eprintln!("TAWARA protect {}: {e:?}", path.display()))
            .is_ok()
    }
}

/// Sets `NSURLIsExcludedFromBackupKey` on the directory, so neither iCloud
/// nor a device backup holds it.
fn exclude_from_backup(dir: &Path) -> bool {
    // SAFETY: Foundation calls with valid objects and Foundation's own key.
    unsafe {
        let url = NSURL::fileURLWithPath_isDirectory(&ns_path(dir), true);
        let yes = NSNumber::new_bool(true);
        url.setResourceValue_forKey_error(Some(&yes), NSURLIsExcludedFromBackupKey)
            .map_err(|e| eprintln!("TAWARA exclude {}: {e:?}", dir.display()))
            .is_ok()
    }
}

fn ns_path(path: &Path) -> Retained<NSString> {
    NSString::from_str(&path.to_string_lossy())
}

/// How long a copy stays on the pasteboard: long enough to paste into another
/// application, short enough that a copied destination or transaction does
/// not linger.
const PASTEBOARD_LIFETIME: Duration = Duration::from_secs(120);

/// Copies `text` to the pasteboard, local to this device (so it does not sync
/// to the user's other devices) and expiring after `PASTEBOARD_LIFETIME`.
/// iced's clipboard sets neither option, so Tawara writes the pasteboard
/// itself. Signed spends get the same treatment, since they can move funds
/// while their reservation is open.
fn copy(text: &str) {
    // The uniform type identifier of plain UTF-8 text.
    const TEXT: &str = "public.utf8-plain-text";
    let options = pasteboard_options();
    // SAFETY: Foundation and UIKit calls with valid objects, on the main
    // thread, where iced runs the application's update.
    unsafe {
        let kind = NSString::from_str(TEXT);
        let string = Retained::into_super(Retained::into_super(NSString::from_str(text)));
        let item = NSDictionary::from_vec(&[&*kind], vec![string]);
        let items = NSArray::from_vec(vec![item]);
        UIPasteboard::generalPasteboard().setItems_options(&items, &options);
    }
    eprintln!(
        "TAWARA clipboard: local only, expiring after {} s",
        PASTEBOARD_LIFETIME.as_secs()
    );
}

/// The options every copy carries: local to this device, and expiring
/// `PASTEBOARD_LIFETIME` from now. Built separately so the test can inspect
/// them, since the simulator cannot observe the local-only option.
fn pasteboard_options() -> Retained<NSDictionary<NSString, AnyObject>> {
    // SAFETY: Foundation constructors, and UIKit's own option keys, which are
    // immutable constants.
    unsafe {
        let local_only = Retained::into_super(Retained::into_super(Retained::into_super(
            NSNumber::new_bool(true),
        )));
        let expires = Retained::into_super(Retained::into_super(
            NSDate::dateWithTimeIntervalSinceNow(PASTEBOARD_LIFETIME.as_secs_f64()),
        ));
        NSDictionary::from_vec(
            &[
                UIPasteboardOptionLocalOnly,
                UIPasteboardOptionExpirationDate,
            ],
            vec![local_only, expires],
        )
    }
}

/// The share sheet, through `UIActivityViewController`. `name` is unused:
/// the sheet takes the text itself, and a subject would need an item
/// provider. The application cannot learn what was done with the sheet. `Ok`
/// means UIKit was asked to present it, not that it was shown; see
/// [`present_share`].
fn share(_name: &str, text: &str) -> Result<(), String> {
    let result = present_share(text);
    match &result {
        Ok(()) => eprintln!("TAWARA share: sheet asked for"),
        Err(error) => eprintln!("TAWARA share: {error}"),
    }
    result
}

/// Presents the sheet from the top of the key window's controllers, the same
/// window `cover` covers. The app is iPhone only (UIDeviceFamily 1), so no
/// popover anchor is needed.
///
/// `UIActivityViewController` delays its presentation until its content is
/// ready, so UIKit gives no immediate signal that the sheet will be shown.
/// A second sheet presented while the first is still waiting is refused by
/// UIKit with only a console message, and its completion never runs.
///
/// So the detectable refusals are checked first, and a second sheet (Save
/// artifact pressed twice) is refused here, so the wallet hears of it. A
/// later UIKit refusal is not reported to the caller; it is logged as "never
/// shown" at the next press after `OPENING_LIMIT`. The completion clears the
/// opening state and logs that the sheet was shown.
fn present_share(text: &str) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    if let Some((_, asked)) = OPENING.get() {
        if asked.elapsed() < OPENING_LIMIT {
            return Err("the share sheet is still opening".to_owned());
        }
        eprintln!("TAWARA share: the last sheet was never shown");
        OPENING.set(None);
    }
    // SAFETY: UIKit calls on the main thread with valid objects. The window
    // and its controllers are UIKit's own, read on the main thread. The
    // completion block runs on the main thread, where `OPENING` lives.
    unsafe {
        let item: Retained<AnyObject> =
            Retained::into_super(Retained::into_super(NSString::from_str(text)));
        let items = NSArray::from_vec(vec![item]);
        let sheet = UIActivityViewController::initWithActivityItems_applicationActivities(
            mtm.alloc(),
            &items,
            None,
        );
        // Exclude the sheet's own Copy, which would put the text on the
        // pasteboard without the local-only flag and expiry that `copy` sets.
        let excluded = NSArray::from_vec(vec![UIActivityTypeCopyToPasteboard.copy()]);
        sheet.setExcludedActivityTypes(Some(&excluded));
        let window = key_window(mtm).ok_or("no key window")?;
        let mut top = window
            .rootViewController()
            .ok_or("no root view controller")?;
        while let Some(presented) = top.presentedViewController() {
            top = presented;
        }
        if top.isBeingPresented() || top.isBeingDismissed() {
            return Err("another screen is still opening or closing".to_owned());
        }
        if !top
            .viewIfLoaded()
            .is_some_and(|view| view.window().is_some())
        {
            return Err("the app's view is not on screen".to_owned());
        }
        let number = SHEETS.get() + 1;
        SHEETS.set(number);
        OPENING.set(Some((number, Instant::now())));
        let shown = RcBlock::new(move || {
            if OPENING.get().is_some_and(|(opening, _)| opening == number) {
                OPENING.set(None);
            }
            eprintln!("TAWARA share: sheet shown");
        });
        top.presentViewController_animated_completion(&sheet, true, Some(&shown));
    }
    Ok(())
}

/// The key window. `keyWindow` is deprecated because it picks one key window
/// across all connected scenes; Tawara has a single scene
/// (`UIApplicationSupportsMultipleScenes` is false) and is iPhone only, so
/// there is only one window to pick.
fn key_window(mtm: MainThreadMarker) -> Option<Retained<UIWindow>> {
    // SAFETY: a UIKit call on the main thread.
    #[allow(deprecated)]
    unsafe {
        UIApplication::sharedApplication(mtm).keyWindow()
    }
}

/// Observes `UIApplication`'s and `UIScreen`'s notifications on the main
/// queue. On iOS Tawara acts on these directly rather than on iced's
/// lifecycle events. The last two report screenshots and screen capture for
/// the recovery phrase's screen. The observers live as long as the process.
fn observe(mtm: MainThreadMarker) {
    type Notice = (&'static NSNotificationName, fn(MainThreadMarker));
    // SAFETY: the notification names are UIKit's own constants.
    let notices: [Notice; 5] = unsafe {
        [
            (UIApplicationWillResignActiveNotification, cover),
            (
                UIApplicationDidEnterBackgroundNotification,
                entered_background,
            ),
            (UIApplicationDidBecomeActiveNotification, uncover),
            (UIApplicationUserDidTakeScreenshotNotification, screenshot),
            (UIScreenCapturedDidChangeNotification, captured_changed),
        ]
    };
    // SAFETY: the main queue and the default center are always valid.
    let (center, queue) = unsafe {
        (
            NSNotificationCenter::defaultCenter(),
            NSOperationQueue::mainQueue(),
        )
    };
    for (name, act) in notices {
        let block = RcBlock::new(move |_: NonNull<NSNotification>| act(mtm));
        // SAFETY: the block is called on the main queue, as `act` needs.
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(name),
                None,
                Some(&queue),
                &block,
            )
        };
        std::mem::forget(observer);
    }
}

/// The application is about to become inactive (the app switcher, a call,
/// Control Center): covers the window, so the app-switcher snapshot shows
/// nothing of the wallet. The logged line says whether a cover was added;
/// the simulator check in platform/ios/tawara.sh reads it, so a missing key
/// window cannot pass for a cover.
fn cover(mtm: MainThreadMarker) {
    COVER.with_borrow_mut(|cover| {
        if cover.is_some() {
            eprintln!("TAWARA lifecycle: inactive; already covered");
            return;
        }
        // SAFETY: UIKit calls on the main thread with valid objects.
        unsafe {
            let window = key_window(mtm);
            // The window is shown only in a window scene; logged for
            // platform/ios/tawara.sh's check.
            let in_scene = window.as_ref().is_some_and(|w| w.windowScene().is_some());
            eprintln!("TAWARA scene: key window in scene={in_scene}");
            let Some(window) = window else {
                eprintln!("TAWARA lifecycle: inactive; no cover: no key window");
                return;
            };
            let view = UIView::initWithFrame(mtm.alloc(), window.bounds());
            let [r, g, b] = background();
            let colour = UIColor::colorWithRed_green_blue_alpha(r, g, b, 1.0);
            view.setBackgroundColor(Some(&colour));
            view.setAutoresizingMask(
                UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight,
            );
            window.addSubview(&view);
            *cover = Some(view);
            eprintln!("TAWARA lifecycle: inactive; cover added");
        }
    });
}

/// Active again: removes the cover. The wallet is locked if the application
/// was in the background meanwhile.
fn uncover(_: MainThreadMarker) {
    if let Some(view) = COVER.with_borrow_mut(Option::take) {
        // SAFETY: UIKit call on the main thread with a valid view.
        unsafe { view.removeFromSuperview() };
    }
    eprintln!("TAWARA lifecycle: active; uncovered");
}

/// In the background: the wallet locks, and the process keeps running until
/// the lock is done. The worker locks after its current command (which a
/// node request or a spend submission can hold), and the interface drops
/// typed input and any recovery phrase on the main thread, but iOS may
/// suspend the process as soon as this returns. So background time is
/// requested from UIKit and ended once the interface has taken the worker's
/// answer (`Leaving`). If the time runs out first, the process exits rather
/// than be suspended with the store's key in memory; the next start is
/// locked and nothing persistent is lost.
fn entered_background(mtm: MainThreadMarker) {
    eprintln!("TAWARA lifecycle: background; locking");
    let task = Arc::new(Task::default());
    let expired = {
        let task = Arc::clone(&task);
        RcBlock::new(move || {
            if !task.locked.load(Ordering::SeqCst) {
                eprintln!("TAWARA lifecycle: background time ran out before the lock; ending");
                std::process::exit(0);
            }
            // SAFETY: UIKit calls the expiration handler on the main thread.
            task.end(unsafe { MainThreadMarker::new_unchecked() });
        })
    };
    // SAFETY: a UIKit call on the main thread; the name is a valid string
    // and the handler a block UIKit copies.
    let id = unsafe {
        UIApplication::sharedApplication(mtm).beginBackgroundTaskWithName_expirationHandler(
            Some(&NSString::from_str("Tawara: locking the wallet")),
            Some(&expired),
        )
    };
    task.id.store(id, Ordering::SeqCst);
    // Stop any scan first, so no camera runs in the background.
    scan::background();
    let leaving = tawara_app::left_foreground();

    // SAFETY: UIBackgroundTaskInvalid is UIKit's own constant.
    if id == unsafe { UIBackgroundTaskInvalid } {
        // No background time: waiting here would block the main thread,
        // which the interface's wipe needs, so exit as on expiry.
        drop(leaving);
        eprintln!("TAWARA lifecycle: no background time to finish the lock; ending");
        std::process::exit(0);
    }

    let spawned = std::thread::Builder::new()
        .name("tawara-background".into())
        .spawn(move || {
            while !leaving.wait(Duration::from_secs(1)) {
                if task.ended.load(Ordering::SeqCst) {
                    return;
                }
            }
            task.locked.store(true, Ordering::SeqCst);
            eprintln!("TAWARA lifecycle: background; locked");
            // UIApplication belongs to the main thread: end the task there.
            let block = RcBlock::new(move || {
                // SAFETY: the main queue runs its operations on the main
                // thread.
                task.end(unsafe { MainThreadMarker::new_unchecked() });
            });
            // SAFETY: the main queue is always valid, and the block is one
            // the queue copies.
            unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&block) };
        });
    if spawned.is_err() {
        // No thread to wait for the lock: exit as on expiry.
        eprintln!("TAWARA lifecycle: no thread to wait for the lock; ending");
        std::process::exit(0);
    }
}

/// One stretch of background time, shared by its expiration handler and the
/// thread waiting for the lock.
#[derive(Default)]
struct Task {
    id: AtomicUsize,
    locked: AtomicBool,
    ended: AtomicBool,
}

impl Task {
    /// Ends the background time once, on the lock or the expiration,
    /// whichever comes first.
    fn end(&self, mtm: MainThreadMarker) {
        if !self.ended.swap(true, Ordering::SeqCst) {
            // SAFETY: a UIKit call on the main thread with the identifier
            // UIKit gave.
            unsafe {
                UIApplication::sharedApplication(mtm)
                    .endBackgroundTask(self.id.load(Ordering::SeqCst));
            }
        }
    }
}

/// A screenshot was taken. UIKit posts this after the capture, so it cannot
/// be prevented; the application warns when the recovery phrase was shown.
fn screenshot(_: MainThreadMarker) {
    eprintln!("TAWARA capture: screenshot taken");
    tawara_app::screenshot_taken();
}

/// The screen started or stopped being recorded or mirrored. The application
/// hides the recovery phrase while the screen is captured. Best effort: iOS
/// can report the screen as not captured while a recording runs.
fn captured_changed(mtm: MainThreadMarker) {
    let captured = is_captured_on(mtm);
    eprintln!("TAWARA capture: captured={captured}");
    tawara_app::capture_changed(captured);
}

/// The screen's capture state at start, through `Host::captured`. Off the
/// main thread it reads as not captured; the next change still reaches the
/// application.
fn is_captured() -> bool {
    let captured = MainThreadMarker::new().is_some_and(is_captured_on);
    eprintln!("TAWARA capture: captured={captured} at start");
    captured
}

/// Whether the main screen is captured by recording, mirroring or AirPlay
/// (`UIScreen.mainScreen.isCaptured`). Both calls are deprecated; the
/// replacement, `UITraitCollection.sceneCaptureState`, needs iOS 17, and the
/// minimum here is iOS 16.0.
#[allow(deprecated)]
fn is_captured_on(mtm: MainThreadMarker) -> bool {
    // SAFETY: UIKit calls on the main thread, where the screen is read.
    unsafe { UIScreen::mainScreen(mtm).isCaptured() }
}

/// The application's background colour, as `UIColor` takes it.
fn background() -> [f64; 3] {
    let c = tawara_app::theme::color::BG_APP;
    [f64::from(c.r), f64::from(c.g), f64::from(c.b)]
}

#[cfg(test)]
mod tests {
    use objc2::{ClassType, msg_send};

    use super::*;

    /// Run on the simulator by platform/ios/tawara.sh, which reads the
    /// pasteboard with `simctl pbpaste` during the copy's lifetime and after
    /// it. The local-only option cannot be observed there, so the test first
    /// asserts the options hold exactly local-only = YES and an expiry about
    /// `PASTEBOARD_LIFETIME` away. It then waits out the lifetime (over two
    /// minutes). `--test-threads=1` keeps it on the main thread.
    #[test]
    fn a_copy_for_the_simulator_check() {
        let options = pasteboard_options();
        assert_eq!(options.count(), 2, "only the two options");
        // SAFETY: each value is checked to be of its class before a message
        // of that class is sent to it; the keys are UIKit's own constants.
        unsafe {
            let local_only = options
                .get(UIPasteboardOptionLocalOnly)
                .expect("the local-only option");
            let is_number: bool = msg_send![local_only, isKindOfClass: NSNumber::class()];
            assert!(is_number, "local-only is an NSNumber");
            let on: bool = msg_send![local_only, boolValue];
            assert!(on, "local-only is YES");
            let expires = options
                .get(UIPasteboardOptionExpirationDate)
                .expect("the expiry option");
            let is_date: bool = msg_send![expires, isKindOfClass: NSDate::class()];
            assert!(is_date, "the expiry is an NSDate");
            let left: f64 = msg_send![expires, timeIntervalSinceNow];
            assert!(
                (110.0..=120.0).contains(&left),
                "the expiry is {left} s away"
            );
        }
        copy("tawara-copy-check");
        println!("TAWARA clipboard check: copied");
        std::thread::sleep(PASTEBOARD_LIFETIME + Duration::from_secs(1));
    }
}
