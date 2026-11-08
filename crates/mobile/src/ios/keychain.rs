// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! iOS device authentication and the store password kept in the Keychain behind
//! it. Provides the `Host` functions `device_check`, `keep_password`,
//! `release_password` and `forget_password` used by `ios.rs`, through the
//! Objective-C runtime and the Security and LocalAuthentication frameworks.
//!
//! Each item is a generic password under one service, with the store identifier
//! (32 bytes, lower-case hex) as its account. Its access control requires user
//! presence (Face ID, Touch ID or the passcode), and the item is readable only
//! while the device has a passcode, and only on this device. Keeping and
//! forgetting run on the caller's thread and do not prompt. A release waits for
//! the prompt, so it runs on its own thread and delivers its answer once through
//! the given callback.
//!
//! Diagnostics go to stderr as `TAWARA unlock` lines and never contain a
//! password.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering, compiler_fence};
use std::sync::{Arc, Mutex, PoisonError};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{ClassType, Message, class, msg_send, msg_send_id};
use objc2_foundation::{
    NSArray, NSCopying, NSData, NSDictionary, NSError, NSMutableData, NSNumber, NSString,
};
use tawara_app::host::{Deliver, DeviceCheck, Released, SecretText};

/// The Keychain service of every unlock item, so that no other item of the
/// application is read or deleted by this module.
const SERVICE: &str = "com.adequatelimited.tawara.unlock";

/// `kSecAccessControlUserPresence` (SecAccessControl.h).
/// `SecAccessControlCreateFlags` is a `CFOptionFlags`, `unsigned long` on 64-bit
/// iOS.
const USER_PRESENCE: u64 = 1 << 0;

/// A Security status (`OSStatus`, SecBase.h).
type OSStatus = i32;

/// `errSecSuccess` (SecBase.h).
const SUCCESS: OSStatus = 0;
/// The user cancelled authentication (`errSecUserCanceled`).
const USER_CANCELED: OSStatus = -128;
/// Authentication failed (`errSecAuthFailed`).
const AUTH_FAILED: OSStatus = -25293;
/// No item matches the query (`errSecItemNotFound`).
const ITEM_NOT_FOUND: OSStatus = -25300;

/// `LAPolicyDeviceOwnerAuthentication` (passcode or biometry) and
/// `LAPolicyDeviceOwnerAuthenticationWithBiometrics` (LAPublicDefines.h).
const POLICY_PASSCODE: isize = 2;
const POLICY_BIOMETRY: isize = 1;

/// `LABiometryTypeTouchID` and `LABiometryTypeFaceID` (LABiometryType.h).
const BIOMETRY_TOUCH_ID: isize = 1;
const BIOMETRY_FACE_ID: isize = 2;

/// A dictionary of Keychain attributes, keyed by Security's own constants.
type Dictionary = NSDictionary<NSString, AnyObject>;

/// Set while a release is in progress, so that a second request does not stack
/// a second prompt.
static ASKING: AtomicBool = AtomicBool::new(false);

// Links LocalAuthentication so that `LAContext` exists at run time; `class!`
// panics for a class that is not loaded.
#[link(name = "LocalAuthentication", kind = "framework")]
unsafe extern "C" {}

#[link(name = "Security", kind = "framework")]
#[allow(non_snake_case, non_upper_case_globals)]
unsafe extern "C" {
    static kSecClass: &'static NSString;
    static kSecClassGenericPassword: &'static NSString;
    static kSecAttrService: &'static NSString;
    static kSecAttrAccount: &'static NSString;
    static kSecAttrAccessControl: &'static NSString;
    static kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly: &'static NSString;
    static kSecValueData: &'static NSString;
    static kSecReturnData: &'static NSString;
    static kSecReturnAttributes: &'static NSString;
    static kSecMatchLimit: &'static NSString;
    static kSecMatchLimitOne: &'static NSString;
    static kSecMatchLimitAll: &'static NSString;
    static kSecUseAuthenticationContext: &'static NSString;

    fn SecItemAdd(attributes: *const c_void, result: *mut *const c_void) -> OSStatus;
    fn SecItemCopyMatching(query: *const c_void, result: *mut *const c_void) -> OSStatus;
    fn SecItemDelete(query: *const c_void) -> OSStatus;
    fn SecAccessControlCreateWithFlags(
        allocator: *const c_void,
        protection: *const c_void,
        flags: u64,
        error: *mut *const c_void,
    ) -> *const c_void;
    fn SecCopyErrorMessageString(status: OSStatus, reserved: *mut c_void) -> *const c_void;
}

/// The device authentication currently available, or `None` when the device
/// has no passcode. Not cached.
pub(super) fn device_check() -> Option<DeviceCheck> {
    // SAFETY: `LAContext` is a class of the framework linked above, and `new`
    // returns a new object that `Retained` owns. `canEvaluatePolicy:error:`
    // shows no prompt, and its error out-pointer may be null.
    unsafe {
        let context: Retained<AnyObject> = msg_send_id![class!(LAContext), new];
        let passcode: bool = msg_send![
            &*context,
            canEvaluatePolicy: POLICY_PASSCODE,
            error: ptr::null_mut::<AnyObject>()
        ];
        if !passcode {
            return None;
        }
        let biometry: bool = msg_send![
            &*context,
            canEvaluatePolicy: POLICY_BIOMETRY,
            error: ptr::null_mut::<AnyObject>()
        ];
        // `biometryType` is valid only after `canEvaluatePolicy` on the same
        // context. Without biometry the check is the passcode.
        let kind: isize = msg_send![&*context, biometryType];
        Some(match (biometry, kind) {
            (true, BIOMETRY_FACE_ID) => DeviceCheck::FaceId,
            (true, BIOMETRY_TOUCH_ID) => DeviceCheck::TouchId,
            _ => DeviceCheck::Passcode,
        })
    }
}

/// Stores `password` under `identifier`, replacing any stored item.
pub(super) fn keep(identifier: &[u8; 32], password: &str) -> Result<(), String> {
    forget()?;
    let control = access_control()?;
    let account = account(identifier);
    // Mutable, so the bytes can be zeroed after the Keychain has copied them.
    let mut copy = NSMutableData::with_bytes(password.as_bytes());
    // The dictionary holds a second reference to the same bytes.
    // SAFETY: `copy` keeps the object alive for the call, and the retain takes one
    // more reference, which `Retained` owns.
    let shared = unsafe { Retained::retain(Retained::as_ptr(&copy).cast_mut()) }
        .ok_or_else(|| "the password could not be copied".to_owned())?;
    // SAFETY: Security's own constants, read as they are.
    let attributes = unsafe {
        dictionary(vec![
            (kSecClass, constant(kSecClassGenericPassword)),
            (kSecAttrService, text(SERVICE)),
            (kSecAttrAccount, text(&account)),
            (kSecAttrAccessControl, control),
            (kSecValueData, any(shared)),
        ])
    };
    // SAFETY: a valid dictionary of attributes, and no result is asked for. The
    // access control carries the protection class, so no `kSecAttrAccessible`
    // is set beside it.
    let status = unsafe { SecItemAdd(as_cf(&attributes), ptr::null_mut()) };
    drop(attributes);
    wipe(copy.bytes_mut());
    if status == SUCCESS {
        eprintln!("TAWARA unlock: password kept");
        Ok(())
    } else {
        eprintln!("TAWARA unlock: password not kept: {status}");
        Err(describe(status))
    }
}

/// Deletes every item under `SERVICE`. Succeeds when there is none.
pub(super) fn forget() -> Result<(), String> {
    // SAFETY: Security's own constants, read as they are.
    let query = unsafe {
        dictionary(vec![
            (kSecClass, constant(kSecClassGenericPassword)),
            (kSecAttrService, text(SERVICE)),
        ])
    };
    // SAFETY: a valid query dictionary.
    let status = unsafe { SecItemDelete(as_cf(&query)) };
    match status {
        SUCCESS | ITEM_NOT_FOUND => Ok(()),
        other => {
            eprintln!("TAWARA unlock: not forgotten: {other}");
            Err(describe(other))
        }
    }
}

/// Prompts for device authentication and reads the stored password. Returns at
/// once; `answer` is called exactly once, later, from a thread of this module
/// (or from the caller's thread if that thread cannot be started).
pub(super) fn release(identifier: &[u8; 32], reason: &str, answer: Deliver) {
    let pending = Pending::new(answer);
    let answering = pending.clone();
    let identifier = *identifier;
    let reason = reason.to_owned();
    let spawned = std::thread::Builder::new()
        .name("tawara-unlock".into())
        .spawn(move || {
            // Answers `Failed` if the thread ends without an answer, e.g. on panic.
            let thread = Answering(answering);
            let released = ask(&identifier, &reason);
            thread.0.finish(released);
        });
    if spawned.is_err() {
        // Answer from here so the callback is still called.
        eprintln!("TAWARA unlock: no thread to ask the check on");
        pending.finish(Released::Failed(
            "the check could not be started".to_owned(),
        ));
    }
}

/// Runs a release on the thread `release` started. Blocks while the prompt is
/// shown.
fn ask(identifier: &[u8; 32], reason: &str) -> Released {
    if reason.trim().is_empty() {
        // The system rejects an empty reason.
        return Released::Failed("the check was given no reason".to_owned());
    }
    let Some(_asking) = Asking::start() else {
        eprintln!("TAWARA unlock: a check is already open");
        return Released::Failed("a check is already open".to_owned());
    };
    let account = account(identifier);
    purge_others(&account);
    let released = read(&account, reason);
    eprintln!("TAWARA unlock: the check answered {}", kind(&released));
    released
}

/// Reads the password under `account`, which prompts for authentication.
fn read(account: &str, reason: &str) -> Released {
    // A fresh context: a context that has already authenticated would skip
    // the prompt.
    // SAFETY: `LAContext` is a class of the linked framework, and `new` returns
    // a new object that `Retained` owns.
    let context: Retained<AnyObject> = unsafe { msg_send_id![class!(LAContext), new] };
    let reason_text = NSString::from_str(reason);
    // SAFETY: `setLocalizedReason:` takes an NSString, which `reason_text` is.
    let _: () = unsafe { msg_send![&*context, setLocalizedReason: &*reason_text] };
    // SAFETY: Security's own constants, read as they are.
    let query = unsafe {
        dictionary(vec![
            (kSecClass, constant(kSecClassGenericPassword)),
            (kSecAttrService, text(SERVICE)),
            (kSecAttrAccount, text(account)),
            (kSecReturnData, yes()),
            (kSecMatchLimit, constant(kSecMatchLimitOne)),
            (kSecUseAuthenticationContext, any(context)),
        ])
    };
    let mut found: *const c_void = ptr::null();
    // SAFETY: a valid query, and `found` is a valid out pointer. On success the
    // result is a +1 CFData, which `Retained` takes over below.
    let status = unsafe { SecItemCopyMatching(as_cf(&query), &mut found) };
    drop(query);
    match status {
        SUCCESS => {
            // SAFETY: the result is the owned CFData that success returns.
            match unsafe { Retained::<NSData>::from_raw(found.cast_mut().cast()) } {
                Some(data) => secret_of(data),
                None => Released::Failed("the Keychain gave back no password".to_owned()),
            }
        }
        USER_CANCELED | AUTH_FAILED => Released::Declined,
        ITEM_NOT_FOUND => Released::Missing,
        other => Released::Failed(describe(other)),
    }
}

/// Converts the stored password into a secret. The bytes are copied into the
/// secret, which zeroes them on drop; `data` belongs to the Keychain and cannot
/// be zeroed, so it is released immediately after the copy.
fn secret_of(data: Retained<NSData>) -> Released {
    let bytes = data.bytes().to_vec();
    drop(data);
    match String::from_utf8(bytes) {
        Ok(mut text) => Released::Password(SecretText::take(&mut text)),
        Err(error) => {
            let mut bytes = error.into_bytes();
            wipe(&mut bytes);
            Released::Failed("the kept password is not text".to_owned())
        }
    }
}

/// Deletes every item under `SERVICE` whose account is not `keep`; items of
/// other store identifiers are never used.
fn purge_others(keep: &str) {
    // SAFETY: Security's own constants, read as they are.
    let query = unsafe {
        dictionary(vec![
            (kSecClass, constant(kSecClassGenericPassword)),
            (kSecAttrService, text(SERVICE)),
            (kSecMatchLimit, constant(kSecMatchLimitAll)),
            (kSecReturnAttributes, yes()),
        ])
    };
    let mut found: *const c_void = ptr::null();
    // SAFETY: a valid query, and `found` is a valid out pointer. On success the
    // result is a +1 array of attribute dictionaries, which `Retained` takes.
    let status = unsafe { SecItemCopyMatching(as_cf(&query), &mut found) };
    drop(query);
    if status == ITEM_NOT_FOUND {
        return;
    }
    if status != SUCCESS {
        eprintln!("TAWARA unlock: the items could not be listed: {status}");
        return;
    }
    // SAFETY: the query asked for attributes of every match, so the result is an
    // NSArray of NSDictionary.
    let Some(items) =
        (unsafe { Retained::<NSArray<Dictionary>>::from_raw(found.cast_mut().cast()) })
    else {
        return;
    };
    for item in items.iter() {
        // SAFETY: Security's own constant, read as it is.
        let Some(name) = (unsafe { item.get(kSecAttrAccount) }).and_then(text_of) else {
            continue;
        };
        if name == keep {
            continue;
        }
        // SAFETY: Security's own constants, read as they are.
        let query = unsafe {
            dictionary(vec![
                (kSecClass, constant(kSecClassGenericPassword)),
                (kSecAttrService, text(SERVICE)),
                (kSecAttrAccount, text(&name)),
            ])
        };
        // SAFETY: a valid query.
        let status = unsafe { SecItemDelete(as_cf(&query)) };
        if status == SUCCESS || status == ITEM_NOT_FOUND {
            eprintln!("TAWARA unlock: an item of another store was deleted");
        } else {
            eprintln!("TAWARA unlock: an item of another store was not deleted: {status}");
        }
    }
}

/// The access control of every item: user presence, readable only while the
/// device has a passcode, and only on this device.
fn access_control() -> Result<Retained<AnyObject>, String> {
    let mut error: *const c_void = ptr::null();
    // SAFETY: Security's own constant, read as it is, and the default allocator
    // (null). `error` is a valid out pointer.
    let control = unsafe {
        SecAccessControlCreateWithFlags(
            ptr::null(),
            (kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly as *const NSString).cast(),
            USER_PRESENCE,
            &mut error,
        )
    };
    if control.is_null() {
        // SAFETY: a failed call returns an owned CFError in `error`, if any.
        let why = unsafe { Retained::<NSError>::from_raw(error.cast_mut().cast()) }.map_or_else(
            || "the system gave no reason".to_owned(),
            |error| error.localizedDescription().to_string(),
        );
        return Err(format!("the access control could not be made: {why}"));
    }
    // SAFETY: a successful call returns an owned access control, which
    // `Retained` takes over.
    unsafe { Retained::from_raw(control.cast_mut().cast()) }
        .ok_or_else(|| "the access control could not be made".to_owned())
}

/// The store identifier as an item account: 32 bytes in lower-case hex. The
/// identifier is not secret.
fn account(identifier: &[u8; 32]) -> String {
    identifier
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Builds a Keychain attribute dictionary from key/value pairs.
fn dictionary(entries: Vec<(&NSString, Retained<AnyObject>)>) -> Retained<Dictionary> {
    let (keys, values): (Vec<&NSString>, Vec<Retained<AnyObject>>) = entries.into_iter().unzip();
    NSDictionary::from_vec(keys.as_slice(), values)
}

/// The dictionary as a `CFDictionaryRef` (toll-free bridged). Valid while the
/// dictionary is alive.
fn as_cf(dictionary: &Dictionary) -> *const c_void {
    (dictionary as *const Dictionary).cast()
}

/// Upcasts an Objective-C object to `AnyObject`, for dictionaries of mixed values.
fn any<T: Message>(object: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an `AnyObject`, and the cast changes
    // nothing but the static type.
    unsafe { Retained::cast(object) }
}

/// A text value for an attribute.
fn text(value: &str) -> Retained<AnyObject> {
    any(NSString::from_str(value))
}

/// Boolean `true` for attributes such as `kSecReturnData`; the same object as
/// `kCFBooleanTrue`.
fn yes() -> Retained<AnyObject> {
    any(NSNumber::new_bool(true))
}

/// A Security constant as a dictionary value.
fn constant(name: &NSString) -> Retained<AnyObject> {
    any(name.copy())
}

/// The attribute value as a string, if it is an `NSString`.
fn text_of(value: &AnyObject) -> Option<String> {
    // SAFETY: `isKindOfClass:` is sent to an object of any class.
    let is_text: bool = unsafe { msg_send![value, isKindOfClass: NSString::class()] };
    // SAFETY: the value is an NSString, as the check above found, so reading it
    // as one is valid.
    is_text.then(|| unsafe { &*(value as *const AnyObject).cast::<NSString>() }.to_string())
}

/// The system's message for a Security status, with its code.
fn describe(status: OSStatus) -> String {
    // SAFETY: Security returns an owned string, or null, which is not taken.
    let words = unsafe { SecCopyErrorMessageString(status, ptr::null_mut()) };
    // SAFETY: a non-null result is an owned CFString, which `Retained` takes over.
    match unsafe { Retained::<NSString>::from_raw(words.cast_mut().cast()) } {
        Some(words) => format!("{words} (Keychain error {status})"),
        None => format!("the Keychain failed with error {status}"),
    }
}

/// A log label for a release result. Never includes the password.
fn kind(released: &Released) -> &'static str {
    match released {
        Released::Password(_) => "a password",
        Released::Declined => "declined",
        Released::Missing => "nothing kept",
        Released::Failed(_) => "failed",
    }
}

/// Overwrites `bytes` with zeros using volatile writes the compiler cannot elide.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid, aligned and exclusive reference to a `u8`.
        unsafe { ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// The callback of one release, taken at most once: by the release thread, or
/// by the fallback when that thread could not be started.
#[derive(Clone)]
struct Pending(Arc<Mutex<Option<Deliver>>>);

impl Pending {
    fn new(answer: Deliver) -> Pending {
        Pending(Arc::new(Mutex::new(Some(answer))))
    }

    /// Delivers `released`, unless an answer was already delivered.
    fn finish(&self, released: Released) {
        let answer = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(answer) = answer {
            answer(released);
        }
    }
}

/// The release thread's handle on a [`Pending`]: on drop without an answer
/// (e.g. after a panic) it delivers `Failed`.
struct Answering(Pending);

impl Drop for Answering {
    fn drop(&mut self) {
        self.0.finish(Released::Failed(
            "the check ended without an answer".to_owned(),
        ));
    }
}

/// Guard for the single release in progress; clears `ASKING` on drop.
struct Asking;

impl Asking {
    /// Claims the release slot, or `None` when a release is already in progress.
    fn start() -> Option<Asking> {
        (!ASKING.swap(true, Ordering::SeqCst)).then_some(Asking)
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        ASKING.store(false, Ordering::SeqCst);
    }
}
