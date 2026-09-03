//! Menu bar status item: an always-visible indicator with live status
//! lines and an "Open Settings…" entry that deep-links into the settings
//! web UI (token carried in the URL fragment, absorbed by the webapp).
//!
//! Everything here is main-thread-only AppKit; the pipeline thread pushes
//! status text through the overlay event loop (`OverlayMsg::Status`).

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{
    define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

struct TargetIvars {
    settings_url: String,
    log_path: PathBuf,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "BetamacsMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            let _ = std::process::Command::new("open")
                .arg(&self.ivars().settings_url)
                .spawn();
        }

        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: Option<&AnyObject>) {
            // Only exists in bundled runs (dev runs log to stderr).
            let path = &self.ivars().log_path;
            if path.exists() {
                let _ = std::process::Command::new("open").arg(path).spawn();
            }
        }
    }
);

impl MenuTarget {
    fn new(settings_url: String, log_path: PathBuf) -> Retained<Self> {
        let this = Self::alloc().set_ivars(TargetIvars {
            settings_url,
            log_path,
        });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct MenuBar {
    /// Dropping an NSStatusItem removes it from the bar; keep it alive.
    _item: Retained<NSStatusItem>,
    status_line: Retained<NSMenuItem>,
    boxes_line: Retained<NSMenuItem>,
    _target: Retained<MenuTarget>,
}

impl MenuBar {
    /// Install the status item. Returns None off the main thread.
    pub fn new(settings_url: String, log_path: PathBuf) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let target = MenuTarget::new(settings_url, log_path);

        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let title = disabled_item(mtm, &format!("betamacs {}", env!("CARGO_PKG_VERSION")));
        let status_line = disabled_item(mtm, "starting…");
        let boxes_line = disabled_item(mtm, "no censor boxes");
        menu.addItem(&title);
        menu.addItem(&status_line);
        menu.addItem(&boxes_line);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&action_item(
            mtm,
            "Open Settings…",
            sel!(openSettings:),
            &target,
        ));
        menu.addItem(&action_item(mtm, "View Log", sel!(openLog:), &target));

        let item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = item.button(mtm) {
            button.setTitle(&NSString::from_str("🛡"));
        }
        item.setMenu(Some(&menu));
        Some(Self {
            _item: item,
            status_line,
            boxes_line,
            _target: target,
        })
    }

    /// First status line: what is being monitored (from the pipeline).
    pub fn set_status(&self, text: &str) {
        self.status_line.setTitle(&NSString::from_str(text));
    }

    /// Second status line: how many censor boxes are on screen right now.
    pub fn set_boxes(&self, n: usize) {
        let text = match n {
            0 => "no censor boxes".to_string(),
            1 => "censoring 1 region".to_string(),
            n => format!("censoring {n} regions"),
        };
        self.boxes_line.setTitle(&NSString::from_str(&text));
    }
}

fn disabled_item(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item.setEnabled(false);
    item
}

fn action_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    target: &MenuTarget,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::new(),
        )
    };
    unsafe { item.setTarget(Some(target)) };
    item
}
