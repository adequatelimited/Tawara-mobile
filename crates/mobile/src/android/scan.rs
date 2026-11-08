// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! Android: the QR scanner, through the NDK's Camera2 and AImageReader, with
//! the camera permission requested and polled through JNI.
//!
//! `android.rs` exposes this scanner through the `Host` fields `scan`,
//! `cancel_scan` and `scan_preview`. The application draws the preview
//! (`ScanPreview::Application`) from the frames this module passes to
//! `tawara_app::host::preview_frame`, at most ten a second, rotated upright
//! for the display's current rotation.
//!
//! One scan runs at a time, on its own thread. The thread waits for the
//! permission when needed, opens the back camera, and decodes each frame's
//! luminance (`qr.rs`). The first text read is returned once through the
//! scan's `done`, after the camera's objects are released. A scan also ends
//! when it is cancelled, when the application moves to the background
//! (`background`), when the window loses focus (a system dialog, the
//! notification shade or Recents), when the camera is lost, and when the
//! application ends (`shutdown`).
//!
//! A NativeActivity gets no `onRequestPermissionsResult`, so the answer is
//! polled with `checkSelfPermission`. The permission dialog takes the
//! window's focus and returns it when it closes, and the answer is read then.
//! A refusal without a dialog (after "don't ask again") changes no focus and
//! is assumed after `NO_DIALOG`. A refusal returns `None`.
//!
//! Threads: `start`, `cancel`, `background` and `shutdown` run on the caller's
//! thread. The scan's thread polls and decodes. The camera's callbacks run on
//! NDK threads: they read a frame's luminance, pass its preview, and hand the
//! frame to the scan's thread through `Inbox`, which keeps only the latest
//! frame. A callback finds its `Inbox` through `CAMERAS` under a numeric key,
//! never a pointer, and `Camera`'s `Drop` removes the key before releasing the
//! reader and every other NDK object.
//!
//! Diagnostics on stderr are `TAWARA scan` lines, one per fact, never a
//! decoded text.

use std::ffi::{CStr, CString, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use jni::objects::{JObjectArray, JString, JValue};
use jni::{jni_sig, jni_str};
use ndk_sys::{
    ACameraCaptureSession, ACameraCaptureSession_close, ACameraCaptureSession_setRepeatingRequest,
    ACameraCaptureSession_stateCallbacks, ACameraCaptureSession_stopRepeating, ACameraDevice,
    ACameraDevice_StateCallbacks, ACameraDevice_close, ACameraDevice_createCaptureRequest,
    ACameraDevice_createCaptureSession, ACameraDevice_request_template, ACameraIdList,
    ACameraManager, ACameraManager_create, ACameraManager_delete,
    ACameraManager_deleteCameraIdList, ACameraManager_getCameraCharacteristics,
    ACameraManager_getCameraIdList, ACameraManager_openCamera, ACameraMetadata,
    ACameraMetadata_const_entry, ACameraMetadata_const_entry__bindgen_ty_1, ACameraMetadata_free,
    ACameraMetadata_getConstEntry, ACameraOutputTarget, ACameraOutputTarget_create,
    ACameraOutputTarget_free, ACaptureRequest, ACaptureRequest_addTarget, ACaptureRequest_free,
    ACaptureSessionOutput, ACaptureSessionOutput_create, ACaptureSessionOutput_free,
    ACaptureSessionOutputContainer, ACaptureSessionOutputContainer_add,
    ACaptureSessionOutputContainer_create, ACaptureSessionOutputContainer_free, AIMAGE_FORMATS,
    AImage, AImage_delete, AImage_getHeight, AImage_getPlaneData, AImage_getPlanePixelStride,
    AImage_getPlaneRowStride, AImage_getWidth, AImageReader, AImageReader_ImageListener,
    AImageReader_acquireLatestImage, AImageReader_delete, AImageReader_getWindow, AImageReader_new,
    AImageReader_setImageListener, ANativeWindow, acamera_metadata_enum_acamera_lens_facing,
    acamera_metadata_enum_acamera_scaler_available_stream_configurations, acamera_metadata_tag,
    camera_status_t, media_status_t,
};
use tawara_app::host::{ScanDone, preview_frame};

use super::with_activity;
use crate::qr;

// The camera functions are in libcamera2ndk, which ndk-sys declares but does
// not link. ndk-sys links `mediandk`, which holds the image reader, through
// its `media` feature.
#[link(name = "camera2ndk")]
unsafe extern "C" {}

/// The camera permission's name.
const CAMERA: &str = "android.permission.CAMERA";
/// `PackageManager.PERMISSION_GRANTED`, as returned by `checkSelfPermission`.
const GRANTED: i32 = 0;
/// The permission request code. The answer is polled, so this is only a label.
const REQUEST_CODE: i32 = 42;
/// `AIMAGE_FORMAT_YUV_420_888`: the frame format, whose first plane is the
/// luminance.
const YUV_420_888: i32 = AIMAGE_FORMATS::AIMAGE_FORMAT_YUV_420_888.0 as i32;
/// `ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS_OUTPUT`: an output
/// configuration, not an input.
const OUTPUT: i32 =
    acamera_metadata_enum_acamera_scaler_available_stream_configurations::ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS_OUTPUT.0 as i32;
/// `ACAMERA_LENS_FACING_BACK`: a back-facing camera.
const LENS_FACING_BACK: u32 = acamera_metadata_enum_acamera_lens_facing::ACAMERA_LENS_FACING_BACK.0;
/// Frames the image reader holds at once: one being read and one arriving.
const FRAMES_HELD: i32 = 2;
/// The minimum interval between previews: ten a second.
const PREVIEW_EVERY: Duration = Duration::from_millis(100);
/// How long a thread waits for a frame or the permission before checking again.
const TICK: Duration = Duration::from_millis(200);
/// How often a scan checks that its window still has focus.
const FOCUS_EVERY: Duration = Duration::from_millis(250);
/// How long a request may go without the window losing focus before it is
/// taken as refused without a dialog.
const NO_DIALOG: Duration = Duration::from_secs(2);
/// The delay after the dialog closes before the answer is read.
const SETTLE: Duration = Duration::from_millis(300);
/// The longest a scan waits for the permission.
const ASKING_LIMIT: Duration = Duration::from_secs(300);

/// The scan under way, from its start until its reply.
struct Scan {
    /// The scan's number, so a thread or callback of an earlier scan cannot
    /// end this one.
    number: u64,
    /// Receives the text, once.
    done: ScanDone,
    /// The camera, once open; `None` while asking for the permission or
    /// opening the camera.
    camera: Option<Camera>,
    /// Shared with the camera's callbacks and the scan's thread.
    inbox: Arc<Inbox>,
}

/// The scan under way, if any. The lock is held only to read or change it,
/// never across a camera call or a reply.
static SCAN: Mutex<Option<Scan>> = Mutex::new(None);

/// The number of the last scan begun.
static SCANS: AtomicU64 = AtomicU64::new(0);

/// The open cameras' inboxes, by callback key. A callback uses its camera's
/// reader only while holding this lock, and `Camera`'s `Drop` removes the key
/// under the same lock before deleting the reader, so no callback reads a
/// deleted reader.
static CAMERAS: Mutex<Vec<(usize, Arc<Inbox>)>> = Mutex::new(Vec::new());

/// The last key given to a camera. Keys are numbers, never pointers, and are
/// never reused, so a late callback finds no inbox.
static KEYS: AtomicUsize = AtomicUsize::new(0);

/// `Host::scan`: starts a scan and returns at once; the text is delivered
/// through `done`, once.
///
/// # Errors
///
/// Returns a user-facing reason when the scan cannot start.
pub(super) fn start(done: ScanDone) -> Result<(), String> {
    // Check for a back camera first; that reads no interface state.
    let manager = Manager::new().ok_or_else(|| refuse("the camera service is not available"))?;
    let choice = back_camera(&manager).map_err(|why| refuse(&why))?;
    drop(manager);
    let number = SCANS.fetch_add(1, Ordering::SeqCst) + 1;
    let inbox = Arc::new(Inbox::new(choice.sensor));
    {
        let mut slot = scan_slot();
        if slot.is_some() {
            return Err(refuse("a scan is already under way"));
        }
        *slot = Some(Scan {
            number,
            done,
            camera: None,
            inbox: Arc::clone(&inbox),
        });
    }
    let Some(granted) = permission_granted() else {
        forget(number);
        return Err(refuse("the camera permission could not be read"));
    };
    if !granted && let Err(why) = request_permission() {
        forget(number);
        return Err(refuse(&why));
    }
    let spawned = thread::Builder::new()
        .name("tawara-scan".to_owned())
        .spawn(move || run(number, !granted, choice, inbox));
    if let Err(error) = spawned {
        forget(number);
        return Err(refuse(&format!(
            "the scanner's thread could not start: {error}"
        )));
    }
    eprintln!("TAWARA scan: started");
    Ok(())
}

/// `Host::cancel_scan`: ends the scan under way, if any, with no text. Any
/// thread.
pub(super) fn cancel() {
    stop("cancelled");
}

/// Ends the scan under way on a move to the background. Runs on the event
/// loop's thread before the application's own `cancel`, so no camera runs in
/// the background.
pub(super) fn background() {
    stop("moved to the background");
}

/// Ends the scan under way when the application ends, so the camera's objects
/// are released before the process exits.
pub(super) fn shutdown() {
    stop("the application ended");
}

/// Ends the scan under way, if there is one, with no text.
fn stop(why: &str) {
    let number = scan_slot().as_ref().map(|scan| scan.number);
    if let Some(number) = number {
        end(number, None, why);
    }
}

/// Ends scan `number` if it is still under way: releases its camera, then
/// replies once with `reading`. Returns whether it ended the scan. Any
/// thread; never called while holding the scan's lock.
fn end(number: u64, reading: Option<Vec<u8>>, why: &str) -> bool {
    let taken = {
        let mut slot = scan_slot();
        if slot.as_ref().is_some_and(|scan| scan.number == number) {
            slot.take()
        } else {
            None
        }
    };
    let Some(Scan {
        done,
        camera,
        inbox,
        ..
    }) = taken
    else {
        return false;
    };
    inbox.end();
    // Release the camera before replying, so the application never shows a
    // page while the camera still runs.
    drop(camera);
    eprintln!("TAWARA scan: ended, {why}");
    done(reading);
    true
}

/// Forgets scan `number`, if still under way, without a reply; used when
/// `start` refuses the scan.
fn forget(number: u64) {
    let taken = {
        let mut slot = scan_slot();
        if slot.as_ref().is_some_and(|scan| scan.number == number) {
            slot.take()
        } else {
            None
        }
    };
    drop(taken);
}

/// Locks the scan under way, for a read or a change only.
fn scan_slot() -> MutexGuard<'static, Option<Scan>> {
    SCAN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Locks the open-camera registry. Held only to find, add or remove an entry,
/// or, in the image callback, while the reader is used.
fn cameras() -> MutexGuard<'static, Vec<(usize, Arc<Inbox>)>> {
    CAMERAS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Registers `inbox` under a new camera key and returns the key.
fn register(inbox: &Arc<Inbox>) -> usize {
    let key = KEYS.fetch_add(1, Ordering::SeqCst) + 1;
    cameras().push((key, Arc::clone(inbox)));
    key
}

/// Removes a released camera's key, so no callback uses its reader or finds
/// its inbox afterwards. Taking the lock waits for any callback still using
/// the reader.
fn unregister(key: usize) {
    cameras().retain(|(entry, _)| *entry != key);
}

/// The inbox of camera `key`, while that camera is open.
fn inbox_of(key: usize) -> Option<Arc<Inbox>> {
    cameras()
        .iter()
        .find(|(entry, _)| *entry == key)
        .map(|(_, inbox)| Arc::clone(inbox))
}

/// Logs a refusal, and returns it for the application.
fn refuse(why: &str) -> String {
    eprintln!("TAWARA scan: {why}");
    why.to_owned()
}

/// The scan's thread: waits for the permission if requested, opens the
/// camera, reads frames until a code is decoded or the scan ends, then ends
/// the scan with the result.
fn run(number: u64, asking: bool, choice: Choice, inbox: Arc<Inbox>) {
    if asking {
        match wait_for_permission(&inbox) {
            Permission::Granted => {}
            Permission::Refused => {
                end(number, None, "the camera access was refused");
                return;
            }
            Permission::Over => return,
        }
    }
    let camera = match open(&inbox, &choice) {
        Ok(camera) => camera,
        Err(why) => {
            end(number, None, &why);
            return;
        }
    };
    if let Some(camera) = attach(number, camera) {
        // The scan ended while the camera was opening.
        drop(camera);
        return;
    }
    let (reading, why) = read_frames(&inbox);
    end(number, reading, why);
}

/// Gives `camera` to scan `number` if it is still under way; otherwise
/// returns the camera to be released.
fn attach(number: u64, camera: Camera) -> Option<Camera> {
    let mut slot = scan_slot();
    match slot.as_mut().filter(|scan| scan.number == number) {
        Some(scan) => {
            scan.camera = Some(camera);
            None
        }
        None => Some(camera),
    }
}

/// Opens the back camera `choice` names and starts its preview.
fn open(inbox: &Arc<Inbox>, choice: &Choice) -> Result<Camera, String> {
    let manager = Manager::new().ok_or_else(|| "the camera service is not available".to_owned())?;
    let mut camera = Camera::new(manager);
    camera.setup(inbox, choice)?;
    Ok(camera)
}

/// The outcome of waiting for the permission.
enum Permission {
    /// The camera may be used.
    Granted,
    /// The user or the system refused it.
    Refused,
    /// The scan ended while it waited.
    Over,
}

/// Polls for the answer to the permission request. The answer is read once
/// the dialog has taken and returned the window's focus; without a dialog, a
/// refusal is assumed after `NO_DIALOG`.
fn wait_for_permission(inbox: &Inbox) -> Permission {
    let asked = Instant::now();
    let mut unfocused = false;
    loop {
        if inbox.pause(TICK) {
            return Permission::Over;
        }
        if permission_granted() == Some(true) {
            return Permission::Granted;
        }
        match window_focused() {
            Some(false) => unfocused = true,
            Some(true) if unfocused => {
                // The dialog has closed; allow a moment for the answer to be
                // recorded.
                if inbox.pause(SETTLE) {
                    return Permission::Over;
                }
                return if permission_granted() == Some(true) {
                    Permission::Granted
                } else {
                    Permission::Refused
                };
            }
            _ => {}
        }
        if !unfocused && asked.elapsed() >= NO_DIALOG {
            return Permission::Refused;
        }
        if asked.elapsed() >= ASKING_LIMIT {
            return Permission::Refused;
        }
    }
}

/// Reads frames until one holds a code, returned with `"a code was read"`, or
/// until the scan ends or the window loses focus, which returns no text and
/// the reason.
fn read_frames(inbox: &Inbox) -> (Option<Vec<u8>>, &'static str) {
    let mut focused = false;
    let mut checked = Instant::now();
    loop {
        match inbox.next(TICK) {
            Next::Frame(frame) => {
                if let Some(text) = qr::read(frame.width, frame.height, &frame.gray) {
                    return (Some(text), "a code was read");
                }
            }
            Next::Idle => {}
            Next::Over => return (None, "the camera stopped"),
        }
        if checked.elapsed() >= FOCUS_EVERY {
            checked = Instant::now();
            match window_focused() {
                Some(true) => focused = true,
                Some(false) if focused => return (None, "the window lost the focus"),
                _ => {}
            }
        }
    }
}

/// What the scan's thread found in the inbox.
enum Next {
    /// An unread frame.
    Frame(Frame),
    /// Nothing arrived within the wait.
    Idle,
    /// The scan has ended or the camera was lost.
    Over,
}

/// State shared by the camera's callbacks and the scan's thread: the latest
/// unread frame, and whether the scan is over.
struct Inbox {
    state: Mutex<State>,
    /// Notified when a frame arrives, the scan ends or the camera is lost.
    moved: Condvar,
    /// The sensor's orientation, used to rotate the preview upright (`qr::turns`).
    sensor: i32,
}

#[derive(Default)]
struct State {
    /// The latest frame until the scan's thread takes it; a newer one replaces it.
    frame: Option<Frame>,
    /// When the last preview was passed to the application.
    previewed: Option<Instant>,
    /// The scan has ended: its thread leaves, and no frame is kept.
    over: bool,
    /// The camera reported an error, or was disconnected.
    lost: bool,
}

/// One frame's luminance: `width` by `height` samples, row by row from the top.
struct Frame {
    width: usize,
    height: usize,
    gray: Vec<u8>,
}

impl Inbox {
    fn new(sensor: i32) -> Inbox {
        Inbox {
            state: Mutex::new(State::default()),
            moved: Condvar::new(),
            sensor,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the scan has ended, so no frame is wanted.
    fn is_over(&self) -> bool {
        self.state().over
    }

    /// Whether a preview is due at `now`; if so, records `now` as the last.
    fn preview_due(&self, now: Instant) -> bool {
        let mut state = self.state();
        let due = !state.over
            && state
                .previewed
                .is_none_or(|last| now.duration_since(last) >= PREVIEW_EVERY);
        if due {
            state.previewed = Some(now);
        }
        due
    }

    /// Keeps `frame` for the scan's thread, replacing any untaken frame.
    /// Nothing is kept once the scan is over.
    fn put(&self, frame: Frame) {
        let mut state = self.state();
        if !state.over {
            state.frame = Some(frame);
            self.moved.notify_all();
        }
    }

    /// Waits up to `within` for a frame or the end of the scan.
    fn next(&self, within: Duration) -> Next {
        let (mut state, _) = self
            .moved
            .wait_timeout_while(self.state(), within, |state| {
                state.frame.is_none() && !state.over && !state.lost
            })
            .unwrap_or_else(PoisonError::into_inner);
        if state.over || state.lost {
            return Next::Over;
        }
        state.frame.take().map_or(Next::Idle, Next::Frame)
    }

    /// Waits up to `within`; returns whether the scan is over or the camera
    /// lost by then.
    fn pause(&self, within: Duration) -> bool {
        let (state, _) = self
            .moved
            .wait_timeout_while(self.state(), within, |state| !state.over && !state.lost)
            .unwrap_or_else(PoisonError::into_inner);
        state.over || state.lost
    }

    /// Marks the scan over: no frame is kept, and the waiting thread leaves.
    fn end(&self) {
        self.state().over = true;
        self.moved.notify_all();
    }

    /// Records that the camera reported an error or was disconnected.
    fn lost(&self) {
        self.state().lost = true;
        self.moved.notify_all();
    }
}

/// An `ACameraManager`, deleted on drop.
struct Manager(*mut ACameraManager);

impl Manager {
    fn new() -> Option<Manager> {
        // SAFETY: the call takes no arguments; a null result is refused below.
        let manager = unsafe { ACameraManager_create() };
        (!manager.is_null()).then_some(Manager(manager))
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        // SAFETY: the manager was made by `ACameraManager_create` and is deleted
        // once, here.
        unsafe { ACameraManager_delete(self.0) };
    }
}

/// A camera open for a scan, and the NDK objects it holds. `Drop` first
/// removes the camera's key from `CAMERAS`, so no callback reads the reader
/// afterwards, then releases the NDK objects in the order the NDK requires:
/// the session and the device, then the reader. Null handles are skipped. The
/// manager, the first field, is released last.
struct Camera {
    manager: Manager,
    device: *mut ACameraDevice,
    session: *mut ACameraCaptureSession,
    request: *mut ACaptureRequest,
    target: *mut ACameraOutputTarget,
    container: *mut ACaptureSessionOutputContainer,
    output: *mut ACaptureSessionOutput,
    reader: *mut AImageReader,
    /// The callbacks' key into `CAMERAS`; zero when none was given.
    key: usize,
    /// The callback structs, on the heap because the NDK may keep their
    /// addresses, and kept until the camera is released.
    listener: Option<Box<AImageReader_ImageListener>>,
    device_callbacks: Option<Box<ACameraDevice_StateCallbacks>>,
    session_callbacks: Option<Box<ACameraCaptureSession_stateCallbacks>>,
}

// SAFETY: the NDK's camera and reader objects are not tied to the thread that
// made them. A camera is used by the scan's thread and dropped by whichever
// thread ends the scan; the raw pointers never leave this module.
unsafe impl Send for Camera {}

impl Camera {
    fn new(manager: Manager) -> Camera {
        Camera {
            manager,
            device: ptr::null_mut(),
            session: ptr::null_mut(),
            request: ptr::null_mut(),
            target: ptr::null_mut(),
            container: ptr::null_mut(),
            output: ptr::null_mut(),
            reader: ptr::null_mut(),
            key: 0,
            listener: None,
            device_callbacks: None,
            session_callbacks: None,
        }
    }

    /// Opens the camera `choice` names and starts its repeating preview
    /// request, with frames delivered to `inbox`.
    ///
    /// # Errors
    ///
    /// Returns which step failed. Objects made before the failure stay in
    /// `self`, so `Drop` releases them.
    fn setup(&mut self, inbox: &Arc<Inbox>, choice: &Choice) -> Result<(), String> {
        self.key = register(inbox);
        let context = ptr::without_provenance_mut::<c_void>(self.key);

        let mut reader: *mut AImageReader = ptr::null_mut();
        // SAFETY: `reader` is a valid out pointer.
        let status = unsafe {
            AImageReader_new(
                choice.width,
                choice.height,
                YUV_420_888,
                FRAMES_HELD,
                &mut reader,
            )
        };
        media_ok(status, "the image reader")?;
        self.reader = reader;

        self.listener = Some(Box::new(AImageReader_ImageListener {
            context,
            onImageAvailable: Some(image_available),
        }));
        let listener = self
            .listener
            .as_deref_mut()
            .map_or(ptr::null_mut(), ptr::from_mut);
        // SAFETY: the reader is live, and the listener is on the heap, kept by
        // this camera until the reader is deleted.
        let status = unsafe { AImageReader_setImageListener(self.reader, listener) };
        media_ok(status, "the image listener")?;

        let mut window: *mut ANativeWindow = ptr::null_mut();
        // SAFETY: the reader is live and `window` is a valid out pointer. The
        // window belongs to the reader, which releases it.
        let status = unsafe { AImageReader_getWindow(self.reader, &mut window) };
        media_ok(status, "the image window")?;

        let mut output: *mut ACaptureSessionOutput = ptr::null_mut();
        // SAFETY: the window is the reader's, and `output` is a valid out pointer.
        let status = unsafe { ACaptureSessionOutput_create(window, &mut output) };
        camera_ok(status, "the session output")?;
        self.output = output;

        let mut container: *mut ACaptureSessionOutputContainer = ptr::null_mut();
        // SAFETY: `container` is a valid out pointer.
        let status = unsafe { ACaptureSessionOutputContainer_create(&mut container) };
        camera_ok(status, "the session outputs")?;
        self.container = container;
        // SAFETY: the container and the output were made above and are live.
        let status = unsafe { ACaptureSessionOutputContainer_add(self.container, self.output) };
        camera_ok(status, "the session outputs")?;

        self.device_callbacks = Some(Box::new(ACameraDevice_StateCallbacks {
            context,
            onDisconnected: Some(device_lost),
            onError: Some(device_error),
        }));
        let callbacks = self
            .device_callbacks
            .as_deref_mut()
            .map_or(ptr::null_mut(), ptr::from_mut);
        let mut device: *mut ACameraDevice = ptr::null_mut();
        // SAFETY: the manager is live, `choice.id` is an NDK-given,
        // NUL-terminated name, the callbacks are on the heap until the device is
        // closed, and `device` is a valid out pointer.
        let status = unsafe {
            ACameraManager_openCamera(self.manager.0, choice.id.as_ptr(), callbacks, &mut device)
        };
        if status == camera_status_t::ACAMERA_ERROR_PERMISSION_DENIED {
            return Err("the camera is not allowed".to_owned());
        }
        camera_ok(status, "the camera")?;
        self.device = device;

        // The NDK refuses a session without callbacks, so pass an empty set.
        self.session_callbacks = Some(Box::new(ACameraCaptureSession_stateCallbacks {
            context,
            onClosed: None,
            onReady: None,
            onActive: None,
        }));
        let callbacks = self
            .session_callbacks
            .as_deref()
            .map_or(ptr::null(), ptr::from_ref);
        let mut session: *mut ACameraCaptureSession = ptr::null_mut();
        // SAFETY: the device and the container are live, the callbacks are on the
        // heap, and `session` is a valid out pointer.
        let status = unsafe {
            ACameraDevice_createCaptureSession(self.device, self.container, callbacks, &mut session)
        };
        camera_ok(status, "the capture session")?;
        self.session = session;

        let mut request: *mut ACaptureRequest = ptr::null_mut();
        // SAFETY: the device is live and `request` is a valid out pointer.
        let status = unsafe {
            ACameraDevice_createCaptureRequest(
                self.device,
                ACameraDevice_request_template::TEMPLATE_PREVIEW,
                &mut request,
            )
        };
        camera_ok(status, "the preview request")?;
        self.request = request;

        let mut target: *mut ACameraOutputTarget = ptr::null_mut();
        // SAFETY: the window is the reader's, and `target` is a valid out pointer.
        let status = unsafe { ACameraOutputTarget_create(window, &mut target) };
        camera_ok(status, "the preview target")?;
        self.target = target;
        // SAFETY: the request and the target were made above and are live.
        let status = unsafe { ACaptureRequest_addTarget(self.request, self.target) };
        camera_ok(status, "the preview target")?;

        let mut sequence: c_int = 0;
        // SAFETY: the session and the request are live, and the count is one.
        // Capture callbacks are optional.
        let status = unsafe {
            ACameraCaptureSession_setRepeatingRequest(
                self.session,
                ptr::null_mut(),
                1,
                &mut self.request,
                &mut sequence,
            )
        };
        camera_ok(status, "the preview")?;
        Ok(())
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        // Unregister first. Taking the registry's lock waits for any callback
        // using the reader; afterwards no callback can reach it, so the reader
        // can be deleted below.
        if self.key != 0 {
            unregister(self.key);
        }
        // SAFETY: each non-null handle was made by this camera and not yet
        // released.
        unsafe {
            if !self.session.is_null() {
                let _ = ACameraCaptureSession_stopRepeating(self.session);
                ACameraCaptureSession_close(self.session);
            }
            if !self.device.is_null() {
                ACameraDevice_close(self.device);
            }
            if !self.request.is_null() {
                ACaptureRequest_free(self.request);
            }
            if !self.target.is_null() {
                ACameraOutputTarget_free(self.target);
            }
            if !self.container.is_null() {
                ACaptureSessionOutputContainer_free(self.container);
            }
            if !self.output.is_null() {
                ACaptureSessionOutput_free(self.output);
            }
            if !self.reader.is_null() {
                // Clear the listener before deleting the reader, as the NDK
                // documents.
                let _ = AImageReader_setImageListener(self.reader, ptr::null_mut());
                AImageReader_delete(self.reader);
            }
        }
    }
}

/// The image callback, on an NDK thread: takes the latest image, copies its
/// luminance, passes the preview when due, and hands the frame on.
///
/// # Safety
///
/// `context` is the listener's key, passed back unchanged by the NDK and
/// never dereferenced. `reader` is the calling reader, used only while
/// `CAMERAS` is locked and the key is registered (see `read_image`).
unsafe extern "C" fn image_available(context: *mut c_void, reader: *mut AImageReader) {
    // A panic must not unwind into the NDK, which would end the process.
    let _ = catch_unwind(AssertUnwindSafe(|| read_image(context.addr(), reader)));
}

/// Reads the latest frame of camera `key` and passes its preview when due.
/// The reader is used only while `CAMERAS` is locked and the key is
/// registered; `Camera::drop` removes the key under that lock before deleting
/// the reader, so the reader is live whenever it is used here.
fn read_image(key: usize, reader: *mut AImageReader) {
    let (inbox, frame) = {
        let cameras = cameras();
        let Some((_, inbox)) = cameras.iter().find(|(entry, _)| *entry == key) else {
            return;
        };
        if inbox.is_over() {
            return;
        }
        // SAFETY: the key is registered, so the reader is live, and it is used
        // under the lock `Camera::drop` takes before deleting the reader.
        let frame = unsafe { take_frame(reader) };
        (Arc::clone(inbox), frame)
    };
    let Some(frame) = frame else {
        return;
    };
    if inbox.preview_due(Instant::now()) {
        let turns = qr::turns(inbox.sensor, display_degrees());
        if let Some((width, height, rgba)) =
            qr::preview(frame.width, frame.height, &frame.gray, turns)
        {
            preview_frame(width, height, rgba);
        }
    }
    inbox.put(frame);
}

/// Copies out the luminance of `reader`'s latest image and releases the
/// image. `None` when there is no image or its plane cannot be read.
///
/// # Safety
///
/// `reader` is a live reader, and it is not deleted while this runs.
unsafe fn take_frame(reader: *mut AImageReader) -> Option<Frame> {
    let mut image: *mut AImage = ptr::null_mut();
    // SAFETY: `reader` is live (see the contract), and `image` is a valid out
    // pointer.
    let status = unsafe { AImageReader_acquireLatestImage(reader, &mut image) };
    if status != media_status_t::AMEDIA_OK || image.is_null() {
        return None;
    }
    let frame = luminance_of(image);
    // SAFETY: `image` was acquired above and is released once, after its plane
    // was copied.
    unsafe { AImage_delete(image) };
    frame
}

/// Copies out an image's first plane (the YUV Y plane). `None` when the plane
/// cannot be read.
fn luminance_of(image: *mut AImage) -> Option<Frame> {
    let (mut width, mut height, mut row, mut pixel) = (0_i32, 0_i32, 0_i32, 0_i32);
    let mut data: *mut u8 = ptr::null_mut();
    let mut len: c_int = 0;
    // SAFETY: `image` is live, and each out pointer is valid for its type.
    let read = unsafe {
        AImage_getWidth(image, &mut width) == media_status_t::AMEDIA_OK
            && AImage_getHeight(image, &mut height) == media_status_t::AMEDIA_OK
            && AImage_getPlaneRowStride(image, 0, &mut row) == media_status_t::AMEDIA_OK
            && AImage_getPlanePixelStride(image, 0, &mut pixel) == media_status_t::AMEDIA_OK
            && AImage_getPlaneData(image, 0, &mut data, &mut len) == media_status_t::AMEDIA_OK
    };
    if !read || data.is_null() {
        return None;
    }
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    let row = usize::try_from(row).ok()?;
    let pixel = usize::try_from(pixel).ok()?;
    let len = usize::try_from(len).ok()?;
    // SAFETY: the plane's `len` bytes are valid until the image is deleted, which
    // is after this copy.
    let plane = unsafe { slice::from_raw_parts(data, len) };
    let gray = qr::luminance(plane, width, height, row, pixel)?;
    Some(Frame {
        width,
        height,
        gray,
    })
}

/// The camera was disconnected: the scan ends with no text.
///
/// # Safety
///
/// `context` is the key the device was opened with, passed back unchanged by
/// the NDK. After the camera removes its key, the callback finds no inbox and
/// does nothing.
unsafe extern "C" fn device_lost(context: *mut c_void, _device: *mut ACameraDevice) {
    if let Some(inbox) = inbox_of(context.addr()) {
        inbox.lost();
    }
}

/// The camera reported an error: the scan ends with no text.
///
/// # Safety
///
/// As for `device_lost`.
unsafe extern "C" fn device_error(context: *mut c_void, _device: *mut ACameraDevice, code: c_int) {
    eprintln!("TAWARA scan: the camera reported error {code}");
    if let Some(inbox) = inbox_of(context.addr()) {
        inbox.lost();
    }
}

/// The device's back camera and the frame size to read it at.
struct Choice {
    /// The camera's Camera2 identifier.
    id: CString,
    /// `ACAMERA_SENSOR_ORIENTATION`: degrees clockwise from upright.
    sensor: i32,
    width: i32,
    height: i32,
}

/// Finds the device's back camera and the frame size to read it at.
///
/// # Errors
///
/// Fails when the camera list cannot be read or there is no back camera.
fn back_camera(manager: &Manager) -> Result<Choice, String> {
    let names = camera_names(manager)?;
    names
        .iter()
        .find_map(|name| back_choice(manager, name))
        .ok_or_else(|| "this device has no back camera".to_owned())
}

/// The device's camera identifiers. The NDK's list is released on every path.
fn camera_names(manager: &Manager) -> Result<Vec<CString>, String> {
    let mut list: *mut ACameraIdList = ptr::null_mut();
    // SAFETY: the manager is live, and `list` is a valid out pointer.
    let status = unsafe { ACameraManager_getCameraIdList(manager.0, &mut list) };
    camera_ok(status, "the camera list")?;
    if list.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: `list` is the NDK's list, live until it is deleted just below.
    let names = unsafe { read_names(list) };
    // SAFETY: the list came from `ACameraManager_getCameraIdList` and is
    // deleted once, after its names were copied.
    unsafe { ACameraManager_deleteCameraIdList(list) };
    Ok(names)
}

/// Copies the names out of a list the NDK made.
///
/// # Safety
///
/// `list` is live: a list from `ACameraManager_getCameraIdList` not yet deleted.
unsafe fn read_names(list: *const ACameraIdList) -> Vec<CString> {
    // SAFETY: see the function's contract. Each name is NUL-terminated and valid
    // until the list is deleted.
    unsafe {
        let count = usize::try_from((*list).numCameras).unwrap_or(0);
        (0..count)
            .map(|index| CStr::from_ptr(*(*list).cameraIds.add(index)).to_owned())
            .collect()
    }
}

/// The choice for camera `name`, if it faces back and offers a usable YUV
/// size.
fn back_choice(manager: &Manager, name: &CString) -> Option<Choice> {
    let mut meta: *mut ACameraMetadata = ptr::null_mut();
    // SAFETY: the manager is live, `name` ends in NUL, and `meta` is a valid out
    // pointer.
    let status =
        unsafe { ACameraManager_getCameraCharacteristics(manager.0, name.as_ptr(), &mut meta) };
    if status != camera_status_t::ACAMERA_OK || meta.is_null() {
        return None;
    }
    let choice = read_choice(meta, name);
    // SAFETY: the characteristics were made above and are released once; no
    // reference into them is kept.
    unsafe { ACameraMetadata_free(meta) };
    choice
}

/// The choice for the camera with characteristics `meta`, if it faces back
/// and offers a usable YUV size.
fn read_choice(meta: *const ACameraMetadata, name: &CString) -> Option<Choice> {
    let facing = entry(meta, acamera_metadata_tag::ACAMERA_LENS_FACING)?;
    // SAFETY: the lens facing is one byte, and `entry` returns only entries
    // with at least one value.
    let facing = unsafe { *facing.data.u8_ };
    if u32::from(facing) != LENS_FACING_BACK {
        return None;
    }
    let sensor = entry(meta, acamera_metadata_tag::ACAMERA_SENSOR_ORIENTATION)
        // SAFETY: the sensor orientation is an int32, with at least one value.
        .map_or(0, |entry| unsafe { *entry.data.i32_ });
    let (width, height) = qr::frame_size(&yuv_sizes(meta)?)?;
    Some(Choice {
        id: name.clone(),
        sensor,
        width,
        height,
    })
}

/// The camera's `YUV_420_888` output sizes, as width and height.
fn yuv_sizes(meta: *const ACameraMetadata) -> Option<Vec<(i32, i32)>> {
    let configurations = entry(
        meta,
        acamera_metadata_tag::ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS,
    )?;
    let count = usize::try_from(configurations.count).ok()?;
    // SAFETY: the entry holds `count` int32 values, four per configuration
    // (format, width, height, input flag), valid while the characteristics are.
    let values = unsafe { slice::from_raw_parts(configurations.data.i32_, count) };
    let (values, _) = values.as_chunks::<4>();
    Some(
        values
            .iter()
            .filter(|configuration| configuration[0] == YUV_420_888 && configuration[3] == OUTPUT)
            .map(|configuration| (configuration[1], configuration[2]))
            .collect(),
    )
}

/// The camera's entry for `tag`, if it has at least one value. Its data is
/// valid while `meta` is.
fn entry(
    meta: *const ACameraMetadata,
    tag: acamera_metadata_tag,
) -> Option<ACameraMetadata_const_entry> {
    let mut entry = ACameraMetadata_const_entry {
        tag: 0,
        type_: 0,
        count: 0,
        data: ACameraMetadata_const_entry__bindgen_ty_1 { u8_: ptr::null() },
    };
    // SAFETY: `meta` is live, and `entry` is a valid out pointer.
    let status = unsafe { ACameraMetadata_getConstEntry(meta, tag.0, &mut entry) };
    (status == camera_status_t::ACAMERA_OK && entry.count > 0).then_some(entry)
}

/// Whether the camera permission is granted. `None` when the check fails.
fn permission_granted() -> Option<bool> {
    with_activity(|env, activity| {
        let name = env.new_string(CAMERA)?;
        let state = env
            .call_method(
                activity,
                jni_str!("checkSelfPermission"),
                jni_sig!((java.lang.String) -> jint),
                &[JValue::Object(&name)],
            )?
            .i()?;
        Ok(state == GRANTED)
    })
    .ok()
}

/// Requests the camera permission through the activity. The answer is polled
/// by `wait_for_permission`. The permission name goes in a `String[]`, the
/// declared parameter type of `requestPermissions`.
fn request_permission() -> Result<(), String> {
    with_activity(|env, activity| {
        let name = env.new_string(CAMERA)?;
        let names = JObjectArray::<JString>::new(env, 1, &name)?;
        env.call_method(
            activity,
            jni_str!("requestPermissions"),
            jni_sig!(([java.lang.String], jint) -> void),
            &[JValue::Object(&names), JValue::Int(REQUEST_CODE)],
        )?;
        Ok(())
    })
    .map_err(|error| format!("the camera permission could not be asked: {error}"))
}

/// Whether the window has focus; a system dialog takes it. `None` when the
/// check fails.
fn window_focused() -> Option<bool> {
    with_activity(|env, activity| {
        let window = env
            .call_method(
                activity,
                jni_str!("getWindow"),
                jni_sig!(() -> android.view.Window),
                &[],
            )?
            .l()?;
        let decor = env
            .call_method(
                &window,
                jni_str!("getDecorView"),
                jni_sig!(() -> android.view.View),
                &[],
            )?
            .l()?;
        env.call_method(
            &decor,
            jni_str!("hasWindowFocus"),
            jni_sig!(() -> jboolean),
            &[],
        )?
        .z()
    })
    .ok()
}

/// The display's clockwise rotation in degrees: 0, 90, 180 or 270; 0 when the
/// check fails.
fn display_degrees() -> i32 {
    with_activity(|env, activity| {
        let manager = env
            .call_method(
                activity,
                jni_str!("getWindowManager"),
                jni_sig!(() -> android.view.WindowManager),
                &[],
            )?
            .l()?;
        let display = env
            .call_method(
                &manager,
                jni_str!("getDefaultDisplay"),
                jni_sig!(() -> android.view.Display),
                &[],
            )?
            .l()?;
        env.call_method(&display, jni_str!("getRotation"), jni_sig!(() -> jint), &[])?
            .i()
    })
    .map_or(0, |rotation| rotation.rem_euclid(4) * 90)
}

/// `camera_status_t` as a result: `Ok` for `ACAMERA_OK`, otherwise the failed
/// step and its status.
fn camera_ok(status: camera_status_t, what: &str) -> Result<(), String> {
    if status == camera_status_t::ACAMERA_OK {
        Ok(())
    } else {
        Err(format!("{what} failed with camera status {}", status.0))
    }
}

/// `media_status_t` as a result, like `camera_ok`.
fn media_ok(status: media_status_t, what: &str) -> Result<(), String> {
    if status == media_status_t::AMEDIA_OK {
        Ok(())
    } else {
        Err(format!("{what} failed with media status {}", status.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a back camera (an emulator run with `-camera-back none
    /// -camera-front none`), a scan is refused at once and never replies. On a
    /// device with a back camera the test passes trivially. No JNI is needed,
    /// since the camera is checked before the permission.
    #[test]
    fn a_scan_with_no_back_camera_is_refused_at_once() {
        let manager = Manager::new().expect("the camera service is available");
        if back_camera(&manager).is_ok() {
            return;
        }
        drop(manager);
        let (sent, received) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
        let refused = start(Box::new(move |text| {
            let _ = sent.send(text);
        }));
        assert_eq!(
            refused.expect_err("a scan with no back camera is refused"),
            "this device has no back camera"
        );
        assert!(
            received.try_recv().is_err(),
            "a refused scan gives no reply"
        );
    }
}
