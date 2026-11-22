//! iOS file attributes (Foundation): keeping a folder out of iCloud and device backups. Like the
//! clipboard and the controllers, a place allowed to call Objective-C.
#![allow(unsafe_code)]
use objc2::{class, msg_send, rc::Retained, runtime::AnyObject};
use objc2_foundation::NSString;
use std::path::Path;

/// Mark `path` as excluded from backups (`NSURLIsExcludedFromBackupKey`): the prepared game files
/// are gigabytes of Mojang's data that can be made again, not the player's to fill iCloud with.
/// A failure only means they stay in the backup.
pub fn exclude_from_backup(path: &Path) {
    let Ok(path) = std::path::absolute(path) else {
        return;
    };
    let Some(path) = path.to_str() else {
        return;
    };
    let path = NSString::from_str(path);
    let key = NSString::from_str("NSURLIsExcludedFromBackupKey");
    // SAFETY: `fileURLWithPath:` takes an NSString and gives an NSURL; `numberWithBool:` gives an
    // NSNumber; `setResourceValue:forKey:error:` takes an object, an NSString and an optional
    // out-pointer for the error (null here) and returns whether it worked.
    unsafe {
        let url: Option<Retained<AnyObject>> = msg_send![class!(NSURL), fileURLWithPath: &*path];
        let yes: Option<Retained<AnyObject>> = msg_send![class!(NSNumber), numberWithBool: true];
        let (Some(url), Some(yes)) = (url, yes) else {
            return;
        };
        let _ok: bool = msg_send![
            &*url,
            setResourceValue: &*yes,
            forKey: &*key,
            error: std::ptr::null_mut::<*mut AnyObject>(),
        ];
    }
}

/// Open an `https://` or `http://` link in Safari (`UIApplication.sharedApplication.openURL`).
pub fn open_url(uri: &str) {
    if !(uri.starts_with("https://") || uri.starts_with("http://")) {
        return;
    }
    let text = NSString::from_str(uri);
    // SAFETY: `URLWithString:`, `sharedApplication`, `dictionary` and
    // `openURL:options:completionHandler:` are standard UIKit/Foundation calls.
    unsafe {
        let url: Option<Retained<AnyObject>> = msg_send![class!(NSURL), URLWithString: &*text];
        let app: Option<Retained<AnyObject>> =
            msg_send![class!(UIApplication), sharedApplication];
        let opts: Option<Retained<AnyObject>> = msg_send![class!(NSDictionary), dictionary];
        if let (Some(url), Some(app), Some(opts)) = (url, app, opts) {
            let () = msg_send![
                &*app,
                openURL: &*url,
                options: &*opts,
                completionHandler: std::ptr::null::<std::ffi::c_void>(),
            ];
        }
    }
}

/// Open a local folder in the iOS Files app via `shareddocuments://`.
pub fn open_folder(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let Ok(path) = std::path::absolute(dir) else {
        return;
    };
    let Some(path_str) = path.to_str() else {
        return;
    };
    let ns_path = NSString::from_str(path_str);
    // SAFETY: Standard Foundation/UIKit calls to convert a local directory path to a properly
    // percent-encoded `file://` URL string, swap the scheme to `shareddocuments://`, and ask
    // UIApplication to open it in the Files app.
    unsafe {
        let file_url: Option<Retained<AnyObject>> =
            msg_send![class!(NSURL), fileURLWithPath: &*ns_path, isDirectory: true];
        let Some(file_url) = file_url else {
            return;
        };
        let abs: Option<Retained<NSString>> = msg_send![&*file_url, absoluteString];
        let Some(abs) = abs else {
            return;
        };
        let abs_str = abs.to_string();
        let Some(rest) = abs_str.strip_prefix("file://") else {
            return;
        };
        let shared = NSString::from_str(&format!("shareddocuments://{rest}"));
        let url: Option<Retained<AnyObject>> = msg_send![class!(NSURL), URLWithString: &*shared];
        let app: Option<Retained<AnyObject>> =
            msg_send![class!(UIApplication), sharedApplication];
        let opts: Option<Retained<AnyObject>> = msg_send![class!(NSDictionary), dictionary];
        if let (Some(url), Some(app), Some(opts)) = (url, app, opts) {
            let () = msg_send![
                &*app,
                openURL: &*url,
                options: &*opts,
                completionHandler: std::ptr::null::<std::ffi::c_void>(),
            ];
        }
    }
}
