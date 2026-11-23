//! A secure macOS dialog for one user-approved secret delivery.

use std::ptr;

use core_foundation::array::CFArray;
use core_foundation::base::{CFAllocatorRef, CFIndex, CFOptionFlags, CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use farhand_core::{Error, Result};
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

const PROMPT_SECONDS: f64 = 45.0;
const CAUTION_ALERT: CFOptionFlags = 2;
const SECURE_FIRST_FIELD: CFOptionFlags = 1 << 16;
const BUTTON_MASK: CFOptionFlags = 3;
const ALTERNATE_BUTTON: CFOptionFlags = 1;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFUserNotificationAlertHeaderKey: CFStringRef;
    static kCFUserNotificationAlertMessageKey: CFStringRef;
    static kCFUserNotificationDefaultButtonTitleKey: CFStringRef;
    static kCFUserNotificationAlternateButtonTitleKey: CFStringRef;
    static kCFUserNotificationTextFieldTitlesKey: CFStringRef;
    static kCFUserNotificationTextFieldValuesKey: CFStringRef;

    fn CFUserNotificationCreate(
        allocator: CFAllocatorRef,
        timeout: f64,
        flags: CFOptionFlags,
        error: *mut i32,
        dictionary: CFDictionaryRef,
    ) -> CFTypeRef;
    fn CFUserNotificationReceiveResponse(
        notification: CFTypeRef,
        timeout: f64,
        flags: *mut CFOptionFlags,
    ) -> i32;
    fn CFUserNotificationGetResponseValue(
        notification: CFTypeRef,
        key: CFStringRef,
        index: CFIndex,
    ) -> CFStringRef;
    fn CFUserNotificationCancel(notification: CFTypeRef) -> i32;
}

pub(super) async fn prompt(host: &str, workdir: &str, command: &str) -> Result<Zeroizing<String>> {
    let details = format!(
        "Send one line to this remote command's standard input.\n\nHost {host:?}\nWorking directory {workdir:?}\nCommand {command:?}"
    );
    if details.chars().count() > 480 {
        return Err(Error::Invalid(
            "the destination and command must fit in the secret input dialog".into(),
        ));
    }
    static DIALOG: Semaphore = Semaphore::const_new(1);
    let permit = DIALOG
        .try_acquire()
        .map_err(|_| Error::Denied("another FarHand secret input dialog is already open".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        request(&details)
    })
    .await
    .map_err(|_| Error::Denied("the secret input dialog could not finish".into()))?
}

fn request(details: &str) -> Result<Zeroizing<String>> {
    let fields = CFArray::from_CFTypes(&[CFString::new("Secret")]);
    // SAFETY: The keys are Apple's immutable CFStrings. The dictionary
    // retains its keys and values through CFUserNotificationCreate.
    let description = unsafe {
        CFDictionary::from_CFType_pairs(&[
            (
                CFString::wrap_under_get_rule(kCFUserNotificationAlertHeaderKey),
                CFString::new("FarHand requests a secret").into_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kCFUserNotificationAlertMessageKey),
                CFString::new(details).into_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kCFUserNotificationDefaultButtonTitleKey),
                CFString::new("Cancel").into_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kCFUserNotificationAlternateButtonTitleKey),
                CFString::new("Send secret").into_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kCFUserNotificationTextFieldTitlesKey),
                fields.into_CFType(),
            ),
        ])
    };
    let mut error = 0;
    // SAFETY: The dictionary has the documented value types and `error`
    // is writable. A non-null result is owned under the create rule.
    let notification = unsafe {
        CFUserNotificationCreate(
            ptr::null(),
            PROMPT_SECONDS,
            CAUTION_ALERT | SECURE_FIRST_FIELD,
            &mut error,
            description.as_concrete_TypeRef(),
        )
    };
    if notification.is_null() {
        return Err(Error::Denied(format!(
            "macOS could not open the secret input dialog (status {error})"
        )));
    }
    // SAFETY: The notification is non-null and held until the response
    // string has been copied into the zeroizing buffer.
    unsafe {
        let notification = CFType::wrap_under_create_rule(notification);
        let reference = notification.as_CFTypeRef();
        let mut flags = 0;
        if CFUserNotificationReceiveResponse(reference, PROMPT_SECONDS, &mut flags) != 0 {
            CFUserNotificationCancel(reference);
            return Err(Error::Denied(
                "the secret input dialog timed out or closed".into(),
            ));
        }
        if flags & BUTTON_MASK != ALTERNATE_BUTTON {
            return Err(Error::Denied("secret delivery was cancelled".into()));
        }
        let value =
            CFUserNotificationGetResponseValue(reference, kCFUserNotificationTextFieldValuesKey, 0);
        if value.is_null() {
            return Err(Error::Invalid(
                "the secret input dialog returned no input".into(),
            ));
        }
        Ok(Zeroizing::new(
            CFString::wrap_under_get_rule(value).to_string(),
        ))
    }
}
