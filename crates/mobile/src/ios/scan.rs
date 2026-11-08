// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! iOS: the QR scanner, through AVFoundation, with hand-written objc2 0.5
//! bindings. `ios.rs` exposes it through the `Host` fields `scan`,
//! `cancel_scan` and `scan_preview`, which the application uses through
//! `tawara_app::scan`.
//!
//! The scanner is a full-screen modal over the key window, presented like
//! the share sheet (`ios.rs`), with the camera's preview and a Cancel button.
//! The first reading that holds exactly one distinct text is returned once
//! through `done`; then the camera stops and the scanner is dismissed. The
//! text is not validated here: the application trims, bounds and validates it
//! as a destination.
//!
//! Threads: the scanner starts and is cancelled on the main thread, where
//! UIKit runs. The session starts and stops on its own serial queue, since
//! AVFoundation blocks in both calls, and readings arrive on another serial
//! queue. A reply may come from either queue, as `ScanDone` allows. The scan
//! under way is kept in `SCAN`; whichever of a reading, the Cancel button,
//! the shell's cancel or a camera refusal comes first takes it, so the reply
//! is sent once and the camera stops once.
//!
//! Diagnostics on stderr are `TAWARA scan` lines, one per fact, never a
//! decoded text.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject, NSObjectProtocol};
use objc2::{
    ClassType, DeclaredClass, ProtocolType, class, declare_class, extern_protocol, msg_send,
    msg_send_id, mutability, sel,
};
use objc2_foundation::{
    CGPoint, CGRect, CGSize, MainThreadMarker, NSArray, NSCopying, NSData, NSOperationQueue,
    NSString,
};
use objc2_ui_kit::{
    UIButton, UIButtonType, UIColor, UIControlEvents, UIControlState, UIInterfaceOrientationMask,
    UIModalPresentationStyle, UIView, UIViewAutoresizing, UIViewController,
};
use tawara_app::host::ScanDone;

// Link AVFoundation so its classes are loaded when first named; `class!`
// panics for an unloaded class. The three constants are AVFoundation's
// `NSString`s.
#[link(name = "AVFoundation", kind = "framework")]
#[allow(non_upper_case_globals)]
unsafe extern "C" {
    static AVMediaTypeVideo: &'static NSString;
    static AVMetadataObjectTypeQRCode: &'static NSString;
    static AVLayerVideoGravityResizeAspectFill: &'static NSString;
}

// libdispatch (in libSystem): creating a serial queue, and running a C
// function with a context on it, which avoids needing a block.
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut AnyObject;
    fn dispatch_async_f(
        queue: *mut AnyObject,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// `AVAuthorizationStatusNotDetermined` (AVCaptureDevice.h).
const NOT_DETERMINED: isize = 0;
/// `AVAuthorizationStatusAuthorized` (AVCaptureDevice.h). The other statuses
/// are refusals.
const AUTHORIZED: isize = 3;

/// `NSUTF8StringEncoding` (NSString.h): the encoding a reading's text is
/// converted to.
const UTF8: usize = 4;

/// The longest text, in UTF-16 units, that is converted. A QR code holds at
/// most 2953 bytes (version 40, lowest error correction), so anything longer
/// is a misreading.
const LONGEST: usize = 4096;

/// `AVCaptureVideoOrientationPortrait` (AVCaptureSession.h), for connections
/// without a rotation angle (before iOS 17).
const PORTRAIT_ORIENTATION: isize = 1;
/// Portrait as a rotation angle, for iOS 17 and later.
const PORTRAIT_ANGLE: f64 = 90.0;

/// The scan under way, from its start until its reply.
struct Scan {
    /// The scan's number, so the answer to its camera request finds it.
    number: u64,
    /// Receives the text, once.
    done: ScanDone,
    /// The camera, once set up; `None` while asking for access.
    running: Option<Running>,
}

/// The camera of a scan that is set up.
struct Running {
    session: Retained<AnyObject>,
    /// The output that reads the codes. Its address distinguishes this scan's
    /// readings from another's.
    output: Retained<AnyObject>,
}

impl Running {
    /// Whether `address` is the address of this scan's output.
    fn is_output(&self, address: usize) -> bool {
        Retained::as_ptr(&self.output) as usize == address
    }
}

// SAFETY: the session is started and stopped only on the session queue, and
// the output is only compared by address. Both are reference-counted
// Objective-C objects; the scan keeps them until its reply, and the queue
// keeps the session until it is stopped.
unsafe impl Send for Running {}

/// The scan under way, if any. The lock is held only to read or change it,
/// never across a UIKit call or a reply.
static SCAN: Mutex<Option<Scan>> = Mutex::new(None);

/// The number of the last scan begun.
static SCANS: AtomicU64 = AtomicU64::new(0);

/// An Objective-C object kept for the life of the process, so no thread can
/// message a freed one. Each is thread-safe: the two queues, and the
/// stateless target.
struct Kept(Retained<AnyObject>);

// SAFETY: see `Kept`. Only messages the object's threading rules allow are
// sent to it from another thread.
unsafe impl Send for Kept {}
// SAFETY: see `Kept`.
unsafe impl Sync for Kept {}

/// Objects shared by every scan, made on the first scan and kept.
struct Shared {
    /// The serial queue the session starts and stops on.
    session: Kept,
    /// The serial queue readings arrive on.
    readings: Kept,
    /// Receives the readings and the Cancel button's press.
    target: Kept,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

thread_local! {
    /// The scanner's presented controller, while it is up. Main thread only.
    static SHEET: RefCell<Option<Retained<UIViewController>>> = const { RefCell::new(None) };
}

/// [`Host::scan`](tawara_app::Host): starts a scan on the main thread and
/// shows the scanner. `done` receives the first reading's text once, or
/// `None` when the scan ends without one.
///
/// # Errors
///
/// Returns a user-facing reason when the scan cannot start.
pub(super) fn start(done: ScanDone) -> Result<(), String> {
    // Check for a camera first; that reads no interface state.
    let Some(device) = video_device() else {
        eprintln!("TAWARA scan: no camera");
        return Err("this device has no camera".to_owned());
    };
    let mtm = MainThreadMarker::new().ok_or("the scanner can only start on the main thread")?;
    if scan_slot().is_some() {
        return Err("a scan is already under way".to_owned());
    }
    if SHEET.with_borrow(Option::is_some) {
        return Err("the scanner is still closing".to_owned());
    }
    shared()?;
    let number = SCANS.fetch_add(1, Ordering::SeqCst) + 1;
    match authorization() {
        AUTHORIZED => {
            *scan_slot() = Some(Scan {
                number,
                done,
                running: None,
            });
            match open(mtm, &device) {
                Ok(opened) => {
                    attach(number, opened);
                    Ok(())
                }
                Err(why) => {
                    forget(number);
                    Err(why)
                }
            }
        }
        NOT_DETERMINED => {
            *scan_slot() = Some(Scan {
                number,
                done,
                running: None,
            });
            ask(number);
            Ok(())
        }
        _ => {
            eprintln!("TAWARA scan: camera access is off");
            Err("camera access is off for Tawara; allow it in Settings".to_owned())
        }
    }
}

/// `Host::cancel_scan`: ends the scan under way, if any, with no text. Any
/// thread.
pub(super) fn cancel() {
    end_if(|_| true, None, "cancelled");
}

/// Ends the scan under way on a move to the background. Runs on the main
/// thread before the application's own `cancel`, so no camera runs in the
/// background.
pub(super) fn background() {
    end_if(|_| true, None, "moved to the background");
}

/// Ends the scan under way if `ends` accepts it: replies once with `reading`,
/// then stops its camera and dismisses its screen. `why` is logged. Returns
/// whether a scan ended. Any thread.
fn end_if(ends: impl FnOnce(&Scan) -> bool, reading: Option<Vec<u8>>, why: &str) -> bool {
    let scan = {
        let mut slot = scan_slot();
        let ends_it = slot.as_ref().is_some_and(ends);
        if ends_it { slot.take() } else { None }
    };
    let Some(Scan { done, running, .. }) = scan else {
        return false;
    };
    eprintln!("TAWARA scan: ended, {why}");
    done(reading);
    if let Some(Running { session, output }) = running {
        enqueue(Order::Stop(session));
        drop(output);
    }
    dismiss();
    true
}

/// Locks the scan under way, for a read or a change only.
fn scan_slot() -> MutexGuard<'static, Option<Scan>> {
    SCAN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Forgets scan `number`, if still under way, without a reply; used when
/// `start` refuses the scan.
fn forget(number: u64) {
    let mut slot = scan_slot();
    if slot.as_ref().is_some_and(|scan| scan.number == number) {
        slot.take();
    }
}

/// The shared objects, made on the first scan (on the main thread).
fn shared() -> Result<&'static Shared, String> {
    if let Some(shared) = SHARED.get() {
        return Ok(shared);
    }
    let session = serial_queue(c"tawara.scan.session")?;
    let readings = serial_queue(c"tawara.scan.readings")?;
    // SAFETY: `new` returns an owned instance, which `Retained` takes over;
    // the cast changes only the static type.
    let target = unsafe {
        let target: Retained<ScanTarget> = msg_send_id![ScanTarget::class(), new];
        Kept(Retained::cast(target))
    };
    let _ = SHARED.set(Shared {
        session,
        readings,
        target,
    });
    SHARED
        .get()
        .ok_or_else(|| "the scanner could not be made".to_owned())
}

/// A new serial queue, kept for the life of the process.
fn serial_queue(label: &CStr) -> Result<Kept, String> {
    // SAFETY: a valid C string and null attributes, which make a serial queue.
    // The queue is returned owned, and `Retained` takes that reference.
    let queue = unsafe { dispatch_queue_create(label.as_ptr(), ptr::null()) };
    // SAFETY: `queue` is the owned reference that `dispatch_queue_create` returned.
    unsafe { Retained::from_raw(queue) }
        .map(Kept)
        .ok_or_else(|| "a queue for the scanner could not be made".to_owned())
}

/// Work for the session queue, run in order, so a stop requested after a
/// start runs after it.
enum Order {
    Start(Retained<AnyObject>),
    Stop(Retained<AnyObject>),
}

/// Asks the session queue to run `order`.
fn enqueue(order: Order) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    let context = Box::into_raw(Box::new(order)).cast::<c_void>();
    // SAFETY: the queue is a live serial queue kept by `shared`, and
    // `run_order` reclaims the box once, on that queue.
    unsafe {
        dispatch_async_f(
            Retained::as_ptr(&shared.session.0).cast_mut(),
            context,
            run_order,
        );
    }
}

/// Runs one order on the session queue, then releases the session reference
/// it carries.
extern "C" fn run_order(context: *mut c_void) {
    // SAFETY: `enqueue` made `context` from a box, and this is its only use.
    let order = unsafe { Box::from_raw(context.cast::<Order>()) };
    match *order {
        Order::Start(session) => {
            // SAFETY: a session the scan made, started on the session queue.
            unsafe {
                let _: () = msg_send![&*session, startRunning];
            }
        }
        Order::Stop(session) => {
            // SAFETY: a session the scan made, stopped on the session queue.
            unsafe {
                let _: () = msg_send![&*session, stopRunning];
            }
        }
    }
}

/// Dismisses the scanner's screen from any thread: directly on the main
/// thread, otherwise through the main queue.
fn dismiss() {
    if MainThreadMarker::new().is_some() {
        dismiss_here();
    } else {
        let block = RcBlock::new(dismiss_here);
        // SAFETY: the main queue is always valid, and it copies the block.
        unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&block) };
    }
}

/// Dismisses the scanner's screen, if it is up. Main thread only.
fn dismiss_here() {
    if let Some(controller) = SHEET.with_borrow_mut(Option::take) {
        // SAFETY: a UIKit call on the main thread, on the controller presented
        // there.
        unsafe { controller.dismissViewControllerAnimated_completion(true, None) };
    }
}

/// The default camera, or `None` when the device has none (as on the
/// simulator).
fn video_device() -> Option<Retained<AnyObject>> {
    // SAFETY: an AVCaptureDevice class method with the media type constant;
    // it returns nil when there is no camera.
    unsafe {
        msg_send_id![
            class!(AVCaptureDevice),
            defaultDeviceWithMediaType: AVMediaTypeVideo
        ]
    }
}

/// The video capture authorization, as an `AVAuthorizationStatus`.
fn authorization() -> isize {
    // SAFETY: an AVCaptureDevice class method with the media type constant.
    unsafe {
        msg_send![
            class!(AVCaptureDevice),
            authorizationStatusForMediaType: AVMediaTypeVideo
        ]
    }
}

/// Requests camera access for scan `number`. The answer arrives on a system
/// thread and is handled by `answered` on the main thread.
fn ask(number: u64) {
    // The argument is Objective-C's BOOL, `Bool` in objc2; `bool` has no
    // block encoding.
    let answer = RcBlock::new(move |granted: Bool| {
        let granted = granted.as_bool();
        let task = RcBlock::new(move || answered(number, granted));
        // SAFETY: the main queue is always valid, and it copies the block.
        unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&task) };
    });
    // SAFETY: an AVCaptureDevice class method with the media type constant and
    // a completion block that AVFoundation copies.
    unsafe {
        let _: () = msg_send![
            class!(AVCaptureDevice),
            requestAccessForMediaType: AVMediaTypeVideo,
            completionHandler: &*answer
        ];
    }
}

/// Handles the access answer for scan `number`, on the main thread. A refusal
/// ends the scan with no text; a grant shows the scanner and starts the
/// camera.
fn answered(number: u64, granted: bool) {
    let waiting = scan_slot()
        .as_ref()
        .is_some_and(|scan| scan.number == number && scan.running.is_none());
    if !waiting {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        end_if(|scan| scan.number == number, None, "not on the main thread");
        return;
    };
    if !granted {
        end_if(|scan| scan.number == number, None, "camera access refused");
        return;
    }
    let Some(device) = video_device() else {
        end_if(|scan| scan.number == number, None, "no camera");
        return;
    };
    match open(mtm, &device) {
        Ok(opened) => {
            attach(number, opened);
        }
        Err(why) => {
            eprintln!("TAWARA scan: {why}");
            end_if(|scan| scan.number == number, None, "could not be shown");
        }
    }
}

/// A scan's camera and screen, once made.
struct Opened {
    session: Retained<AnyObject>,
    output: Retained<AnyObject>,
    controller: Retained<UIViewController>,
}

/// Makes the camera and the scanner's screen, and presents the screen over
/// the top of the key window's controllers, like the share sheet (`ios.rs`).
/// The camera is started by `attach`, if the scan is still under way.
fn open(mtm: MainThreadMarker, device: &AnyObject) -> Result<Opened, String> {
    let shared = shared()?;
    let (top, bounds, inset) = presenting(mtm)?;
    let (session, output) = camera(device, shared)?;
    let controller = screen(mtm, bounds, inset, shared, &session)?;
    // SAFETY: UIKit calls on the main thread, with the controller made above.
    let shown = unsafe {
        top.presentViewController_animated_completion(&controller, true, None);
        top.presentedViewController()
    };
    let presented = shown.is_some_and(|presented| {
        ptr::eq(Retained::as_ptr(&presented), Retained::as_ptr(&controller))
    });
    if !presented {
        // SAFETY: a UIKit call on the main thread, in case UIKit queued the
        // presentation after all.
        unsafe { controller.dismissViewControllerAnimated_completion(true, None) };
        return Err("the scanner could not be shown".to_owned());
    }
    Ok(Opened {
        session,
        output,
        controller,
    })
}

/// The top of the key window's controllers, to present the scanner from,
/// with the window's bounds and top safe-area inset for placing the Cancel
/// button.
///
/// # Errors
///
/// Fails when the screen cannot take a presentation now, by the same checks
/// as `present_share` in `ios.rs`.
fn presenting(mtm: MainThreadMarker) -> Result<(Retained<UIViewController>, CGRect, f64), String> {
    let window = super::key_window(mtm).ok_or("no key window")?;
    // SAFETY: UIKit reads on the main thread, of UIKit-owned objects.
    unsafe {
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
        Ok((top, window.bounds(), window.safeAreaInsets().top))
    }
}

/// A capture session for `device`, with a QR metadata output delivering to
/// the readings queue. Each input and output is checked before it is added,
/// since AVFoundation raises an exception for one it cannot take, and an
/// uncaught exception ends the application.
fn camera(
    device: &AnyObject,
    shared: &Shared,
) -> Result<(Retained<AnyObject>, Retained<AnyObject>), String> {
    // SAFETY: AVFoundation's constructors and the session's calls, on the main
    // thread, with objects made here.
    unsafe {
        let session: Retained<AnyObject> = msg_send_id![class!(AVCaptureSession), new];
        let input: Option<Retained<AnyObject>> = msg_send_id![
            class!(AVCaptureDeviceInput),
            deviceInputWithDevice: device,
            error: ptr::null_mut::<*mut AnyObject>()
        ];
        let input = input.ok_or("the camera could not be read")?;
        let can_input: bool = msg_send![&*session, canAddInput: &*input];
        if !can_input {
            return Err("the session takes no camera".to_owned());
        }
        let _: () = msg_send![&*session, addInput: &*input];
        let output: Retained<AnyObject> = msg_send_id![class!(AVCaptureMetadataOutput), new];
        let can_output: bool = msg_send![&*session, canAddOutput: &*output];
        if !can_output {
            return Err("the session takes no code reader".to_owned());
        }
        let _: () = msg_send![&*session, addOutput: &*output];
        // Available types are known only after the output is added, and
        // setting an unavailable type raises an exception.
        let offered: Retained<AnyObject> = msg_send_id![&*output, availableMetadataObjectTypes];
        let qr: &NSString = AVMetadataObjectTypeQRCode;
        let reads_qr: bool = msg_send![&*offered, containsObject: qr];
        if !reads_qr {
            return Err("this camera reads no QR codes".to_owned());
        }
        let _: () = msg_send![
            &*output,
            setMetadataObjectsDelegate: &*shared.target.0,
            queue: &*shared.readings.0
        ];
        let types = NSArray::from_vec(vec![qr.copy()]);
        let _: () = msg_send![&*output, setMetadataObjectTypes: &*types];
        Ok((session, output))
    }
}

/// The scanner's portrait-only screen: a full-screen camera preview and a
/// Cancel button at the top right, below the status bar.
fn screen(
    mtm: MainThreadMarker,
    bounds: CGRect,
    inset: f64,
    shared: &Shared,
    session: &AnyObject,
) -> Result<Retained<UIViewController>, String> {
    // SAFETY: UIKit and AVFoundation calls on the main thread, with objects
    // made here and the target `shared` keeps.
    unsafe {
        let preview: Retained<PreviewView> =
            msg_send_id![mtm.alloc::<PreviewView>(), initWithFrame: bounds];
        let view: Retained<UIView> = Retained::into_super(preview);
        view.setAutoresizingMask(
            UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight,
        );
        view.setBackgroundColor(Some(&UIColor::blackColor()));
        // The view's layer is the preview layer (see `PreviewView`). Read
        // through the runtime, since UIKit's binding needs the Core Animation
        // crate.
        let layer: Retained<AnyObject> = msg_send_id![&*view, layer];
        let _: () = msg_send![&*layer, setSession: session];
        let gravity: &NSString = AVLayerVideoGravityResizeAspectFill;
        let _: () = msg_send![&*layer, setVideoGravity: gravity];
        let connection: Option<Retained<AnyObject>> = msg_send_id![&*layer, connection];
        if let Some(connection) = connection {
            portrait(&connection);
        }

        let button = UIButton::buttonWithType(UIButtonType::System, mtm);
        let title = NSString::from_str("Cancel");
        button.setTitle_forState(Some(&title), UIControlState::Normal);
        button.setTintColor(Some(&UIColor::whiteColor()));
        button.addTarget_action_forControlEvents(
            Some(&*shared.target.0),
            sel!(cancelScan:),
            UIControlEvents::UIControlEventTouchUpInside,
        );
        button.setAutoresizingMask(
            UIViewAutoresizing::FlexibleLeftMargin | UIViewAutoresizing::FlexibleBottomMargin,
        );
        button.setFrame(CGRect::new(
            CGPoint::new(bounds.size.width - 16.0 - 96.0, inset + 8.0),
            CGSize::new(96.0, 44.0),
        ));
        view.addSubview(&button);

        let controller: Retained<ScanController> =
            msg_send_id![mtm.alloc::<ScanController>(), init];
        let controller: Retained<UIViewController> = Retained::into_super(controller);
        controller.setView(Some(&view));
        controller
            .setModalPresentationStyle(UIModalPresentationStyle::UIModalPresentationFullScreen);
        Ok(controller)
    }
}

/// Sets a preview connection to portrait: by rotation angle where supported
/// (iOS 17 and later), otherwise by orientation.
fn portrait(connection: &AnyObject) {
    // SAFETY: `respondsToSelector:` is valid on any object, and each setter is
    // sent only when the connection supports it.
    unsafe {
        let angled: bool = msg_send![connection, respondsToSelector: sel!(setVideoRotationAngle:)];
        if angled {
            let supported: bool =
                msg_send![connection, isVideoRotationAngleSupported: PORTRAIT_ANGLE];
            if supported {
                let _: () = msg_send![connection, setVideoRotationAngle: PORTRAIT_ANGLE];
            }
        } else {
            let _: () = msg_send![connection, setVideoOrientation: PORTRAIT_ORIENTATION];
        }
    }
}

/// Attaches `opened` to scan `number` and starts its camera, if the scan is
/// still under way; otherwise dismisses the screen. Returns whether the scan
/// was attached.
fn attach(number: u64, opened: Opened) -> bool {
    let Opened {
        session,
        output,
        controller,
    } = opened;
    let attached = {
        let mut slot = scan_slot();
        match slot.as_mut() {
            Some(scan) if scan.number == number && scan.running.is_none() => {
                scan.running = Some(Running {
                    session: session.clone(),
                    output,
                });
                true
            }
            _ => false,
        }
    };
    if !attached {
        // SAFETY: a UIKit call on the main thread, on the presented controller.
        unsafe { controller.dismissViewControllerAnimated_completion(true, None) };
        return false;
    }
    SHEET.with_borrow_mut(|sheet| *sheet = Some(controller));
    enqueue(Order::Start(session));
    eprintln!("TAWARA scan: started");
    true
}

/// Handles a reading on the readings queue. When it holds exactly one
/// distinct text, that text becomes the reply of the scan whose output
/// produced the reading.
fn reading(output: &AnyObject, codes: &NSArray<AnyObject>) {
    let texts: Vec<Vec<u8>> = codes.iter().filter_map(code_text).collect();
    let Some(text) = one_text(texts) else {
        return;
    };
    let address = output as *const AnyObject as usize;
    end_if(
        |scan| {
            scan.running
                .as_ref()
                .is_some_and(|running| running.is_output(address))
        },
        Some(text),
        "read",
    );
}

/// The reading's text when it holds exactly one distinct text. Two different
/// codes at once are ambiguous, so they are ignored and the camera reads on.
fn one_text(mut texts: Vec<Vec<u8>>) -> Option<Vec<u8>> {
    texts.sort();
    texts.dedup();
    if texts.len() == 1 { texts.pop() } else { None }
}

/// The UTF-8 bytes of a machine-readable code's text
/// (AVMetadataMachineReadableCodeObject's `stringValue`), if it has one.
fn code_text(code: &AnyObject) -> Option<Vec<u8>> {
    // SAFETY: `isKindOfClass:` is valid on any object, with an AVFoundation
    // class.
    let is_code: bool =
        unsafe { msg_send![code, isKindOfClass: class!(AVMetadataMachineReadableCodeObject)] };
    if !is_code {
        return None;
    }
    // SAFETY: a machine-readable code has the `stringValue` getter, which
    // returns an owned or null string.
    let text: Option<Retained<NSString>> = unsafe { msg_send_id![code, stringValue] };
    let text = text?;
    if text.len_utf16() > LONGEST {
        return None;
    }
    // SAFETY: `dataUsingEncoding:` on a string with a valid encoding returns
    // owned data or null.
    let data: Option<Retained<NSData>> = unsafe { msg_send_id![&*text, dataUsingEncoding: UTF8] };
    Some(data?.bytes().to_vec())
}

// The preview view. Its layer class is AVFoundation's preview layer, so the
// camera's frames fill it without a sublayer.
declare_class!(
    struct PreviewView;

    unsafe impl ClassType for PreviewView {
        type Super = UIView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "TawaraScanPreview";
    }

    impl DeclaredClass for PreviewView {}

    unsafe impl PreviewView {
        #[method(layerClass)]
        fn layer_class() -> &'static AnyClass {
            class!(AVCaptureVideoPreviewLayer)
        }
    }
);

// The scanner's controller. Portrait only, so the preview stays upright as
// the device turns; `portrait` sets the connection to match.
declare_class!(
    struct ScanController;

    unsafe impl ClassType for ScanController {
        type Super = UIViewController;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "TawaraScanController";
    }

    impl DeclaredClass for ScanController {}

    unsafe impl ScanController {
        #[method(supportedInterfaceOrientations)]
        fn supported_interface_orientations(&self) -> UIInterfaceOrientationMask {
            UIInterfaceOrientationMask::Portrait
        }
    }
);

extern_protocol!(
    /// AVFoundation's `AVCaptureMetadataOutputObjectsDelegate` (AVCaptureMetadataOutput.h),
    /// to which the camera's readings are given.
    ///
    /// # Safety
    ///
    /// An implementation is an Objective-C object that AVFoundation can message.
    /// The readings are sent on the queue the output was given, so an
    /// implementation must not block on the scanner's session queue.
    #[allow(clippy::missing_safety_doc)]
    unsafe trait AVCaptureMetadataOutputObjectsDelegate: NSObjectProtocol {
        #[optional]
        #[method(captureOutput:didOutputMetadataObjects:fromConnection:)]
        unsafe fn capture_output_did_output_metadata_objects_from_connection(
            &self,
            output: &AnyObject,
            metadata_objects: &NSArray<AnyObject>,
            connection: &AnyObject,
        );
    }

    unsafe impl ProtocolType for dyn AVCaptureMetadataOutputObjectsDelegate {}
);

// Receives the readings and the Cancel button's action. Stateless, and kept
// for the life of the process (`Kept`).
declare_class!(
    struct ScanTarget;

    unsafe impl ClassType for ScanTarget {
        type Super = NSObject;
        type Mutability = mutability::Immutable;
        const NAME: &'static str = "TawaraScanTarget";
    }

    impl DeclaredClass for ScanTarget {}

    unsafe impl NSObjectProtocol for ScanTarget {}

    unsafe impl AVCaptureMetadataOutputObjectsDelegate for ScanTarget {
        #[method(captureOutput:didOutputMetadataObjects:fromConnection:)]
        unsafe fn capture_output_did_output_metadata_objects_from_connection(
            &self,
            output: &AnyObject,
            metadata_objects: &NSArray<AnyObject>,
            _connection: &AnyObject,
        ) {
            reading(output, metadata_objects);
        }
    }

    unsafe impl ScanTarget {
        #[method(cancelScan:)]
        fn cancel_pressed(&self, _sender: &AnyObject) {
            end_if(|_| true, None, "cancelled");
        }
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_distinct_text_is_taken_and_no_other() {
        assert_eq!(one_text(Vec::new()), None);
        assert_eq!(one_text(vec![b"0xA".to_vec()]), Some(b"0xA".to_vec()));
        assert_eq!(
            one_text(vec![b"0xA".to_vec(), b"0xA".to_vec()]),
            Some(b"0xA".to_vec())
        );
        assert_eq!(one_text(vec![b"0xA".to_vec(), b"0xB".to_vec()]), None);
    }

    /// Without a camera (the simulator), a scan is refused at once and never
    /// replies. The camera check precedes the main-thread check, so the test
    /// can run on a harness thread. On a device with a camera it passes
    /// trivially.
    #[test]
    fn a_scan_with_no_camera_is_refused_at_once() {
        if video_device().is_some() {
            return;
        }
        let (sent, received) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
        let refused = start(Box::new(move |text| {
            let _ = sent.send(text);
        }));
        assert_eq!(
            refused.expect_err("a scan with no camera is refused"),
            "this device has no camera"
        );
        assert!(
            received.try_recv().is_err(),
            "a refused scan gives no reply"
        );
    }
}
