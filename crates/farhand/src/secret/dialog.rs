//! Native macOS dialog for a background FarHand MCP server.

use std::ptr;

use core_foundation::array::CFArray;
use core_foundation::base::{CFAllocatorRef, CFIndex, CFOptionFlags, CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use farhand_core::{Error, Result};
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

pub(super) fn request(details: &str) -> Result<Zeroizing<String>> {
    let fields = CFArray::from_CFTypes(&[CFString::new("Password")]);
    // SAFETY: The keys are Apple's immutable CFStrings. The dictionary
    // retains its keys and values through CFUserNotificationCreate.
    let description = unsafe {
        CFDictionary::from_CFType_pairs(&[
            (
                CFString::wrap_under_get_rule(kCFUserNotificationAlertHeaderKey),
                CFString::new("FarHand requests a password").into_CFType(),
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
                CFString::new("Send password").into_CFType(),
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
            "macOS could not open the password dialog (status {error})"
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
                "the password dialog timed out or closed".into(),
            ));
        }
        if flags & BUTTON_MASK != ALTERNATE_BUTTON {
            return Err(Error::Denied("password delivery was cancelled".into()));
        }
        let value =
            CFUserNotificationGetResponseValue(reference, kCFUserNotificationTextFieldValuesKey, 0);
        if value.is_null() {
            return Err(Error::Invalid(
                "the password dialog returned no input".into(),
            ));
        }
        Ok(Zeroizing::new(
            CFString::wrap_under_get_rule(value).to_string(),
        ))
    }
}
