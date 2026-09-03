//! SMAppService bindings for registering betamacsd as a system
//! LaunchDaemon from the app's own bundle (macOS 13+): the privileged
//! bootstrap becomes one admin approval in System Settings → Login
//! Items instead of a sudo script. Raw objc2 message sends — the
//! surface is three calls, not worth a bindings crate.

use anyhow::Result;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSError, NSString};

// Force the framework to link so class!(SMAppService) resolves.
#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
    Other(i64),
}

impl ServiceStatus {
    fn from_raw(raw: i64) -> Self {
        match raw {
            0 => Self::NotRegistered,
            1 => Self::Enabled,
            2 => Self::RequiresApproval,
            3 => Self::NotFound,
            other => Self::Other(other),
        }
    }
}

fn daemon_service(plist_name: &str) -> Retained<AnyObject> {
    let name = NSString::from_str(plist_name);
    unsafe { msg_send![class!(SMAppService), daemonServiceWithPlistName: &*name] }
}

pub fn status(plist_name: &str) -> ServiceStatus {
    let service = daemon_service(plist_name);
    let raw: i64 = unsafe { msg_send![&*service, status] };
    ServiceStatus::from_raw(raw)
}

/// Register the daemon; returns the resulting status. "Requires
/// approval" is the expected first outcome — the caller then points the
/// user at System Settings.
pub fn register(plist_name: &str) -> Result<ServiceStatus> {
    let service = daemon_service(plist_name);
    let mut error: *mut NSError = std::ptr::null_mut();
    let ok: bool = unsafe { msg_send![&*service, registerAndReturnError: &mut error] };
    let status = status(plist_name);
    if !ok && status != ServiceStatus::RequiresApproval && status != ServiceStatus::Enabled {
        let detail = unsafe { error.as_ref() }
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_else(|| "unknown error".into());
        anyhow::bail!("SMAppService registration failed: {detail} (status {status:?})");
    }
    Ok(status)
}

/// Open System Settings → Login Items, where the daemon approval lives.
pub fn open_login_items_settings() {
    unsafe {
        let _: () = msg_send![class!(SMAppService), openSystemSettingsLoginItems];
    }
}
