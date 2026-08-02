//! Censor overlay: a pool of borderless windows, one per censor region,
//! positioned in global logical (point) coordinates. Fill color, border,
//! and capture visibility come from the censor module settings and can be
//! restyled live.
//!
//! Properties per window:
//!   - styled background (default black), no decorations, no shadow,
//!     never focused
//!   - always-on-top, click-through (`set_cursor_hittest(false)`)
//!   - content-protected (`NSWindow.sharingType = .none`) unless
//!     `censor_in_captures` is on, so the boxes are invisible to screen
//!     capture — including our own detector loop
//!   - joins all Spaces, including fullscreen apps, and stays put during
//!     Mission Control (collection behavior flags)
//!
//! winit requires the event loop to run on the main thread on macOS, so the
//! pipeline runs on a worker thread and sends updates through an
//! `EventLoopProxy` user event.

use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId, WindowLevel};

use crate::settings::CensorSettings;

/// A rectangle to black out, in global logical screen points (the same
/// coordinate space as ScreenCaptureKit display origins).
#[derive(Debug, Clone, PartialEq)]
pub struct CensorRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// NudeNet class that caused this box (for the trigger-label overlay).
    pub trigger: &'static str,
    /// Picks this box's overlay text; assigned when the box first appears
    /// and preserved while it moves, so the text stays stable for the
    /// box's whole lifetime.
    pub text_seed: u64,
}

#[derive(Debug)]
pub enum OverlayMsg {
    Regions(Vec<CensorRegion>),
    Style(CensorSettings),
}

/// Handle used by the pipeline/server threads to push updates.
#[derive(Clone)]
pub struct OverlayHandle {
    proxy: EventLoopProxy<OverlayMsg>,
}

impl OverlayHandle {
    /// Replace the set of censor boxes shown on screen.
    pub fn set_regions(&self, regions: Vec<CensorRegion>) -> Result<()> {
        self.proxy
            .send_event(OverlayMsg::Regions(regions))
            .map_err(|_| anyhow::anyhow!("overlay event loop is gone"))
    }

    /// Restyle the boxes (fill, border, capture visibility).
    pub fn set_style(&self, style: CensorSettings) -> Result<()> {
        self.proxy
            .send_event(OverlayMsg::Style(style))
            .map_err(|_| anyhow::anyhow!("overlay event loop is gone"))
    }
}

pub struct OverlayApp {
    windows: Vec<Window>,
    /// Text fields (random text + trigger label) per window, same index.
    chromes: Vec<macos::Chrome>,
    visible: usize,
    /// Last applied region set; identical updates are dropped so a static
    /// scene causes zero window-server traffic (window updates would
    /// otherwise re-trigger SCK change frames in a feedback loop).
    last_regions: Vec<CensorRegion>,
    style: CensorSettings,
}

impl OverlayApp {
    /// Build the event loop and a handle for the worker threads.
    pub fn new(style: CensorSettings) -> Result<(EventLoop<OverlayMsg>, OverlayHandle, Self)> {
        let event_loop = EventLoop::<OverlayMsg>::with_user_event()
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build event loop: {e}"))?;
        let handle = OverlayHandle {
            proxy: event_loop.create_proxy(),
        };
        Ok((
            event_loop,
            handle,
            Self {
                windows: Vec::new(),
                chromes: Vec::new(),
                visible: 0,
                last_regions: Vec::new(),
                style,
            },
        ))
    }

    fn content_protected(&self) -> bool {
        // BETAMACS_NO_PROTECT=1 overrides for debugging, to verify via
        // capture that the boxes actually render.
        !self.style.censor_in_captures && std::env::var_os("BETAMACS_NO_PROTECT").is_none()
    }

    fn ensure_window(&mut self, event_loop: &ActiveEventLoop, i: usize) -> Result<()> {
        while self.windows.len() <= i {
            let attrs = Window::default_attributes()
                .with_title("betamacs censor")
                .with_decorations(false)
                .with_resizable(false)
                .with_active(false)
                .with_visible(false)
                .with_window_level(WindowLevel::AlwaysOnTop)
                .with_content_protected(self.content_protected())
                .with_inner_size(LogicalSize::new(1.0, 1.0));
            let window = event_loop
                .create_window(attrs)
                .map_err(|e| anyhow::anyhow!("failed to create overlay window: {e}"))?;
            let _ = window.set_cursor_hittest(false);
            macos::configure_censor_window(&window);
            macos::apply_style(&window, &self.style);
            self.chromes.push(macos::Chrome::attach(&window));
            self.windows.push(window);
        }
        Ok(())
    }

    fn apply_regions(&mut self, event_loop: &ActiveEventLoop, regions: Vec<CensorRegion>) {
        if regions == self.last_regions {
            return;
        }
        for (i, region) in regions.iter().enumerate() {
            if let Err(e) = self.ensure_window(event_loop, i) {
                tracing::error!("{e}");
                return;
            }
            let window = &self.windows[i];
            let _ = window.request_inner_size(LogicalSize::new(region.width, region.height));
            window.set_outer_position(LogicalPosition::new(region.x, region.y));
            self.chromes[i].update(region, &self.style);
            window.set_visible(true);
            // winit's set_visible uses orderFront, which the window server
            // can ignore for a background (never-activated) app; this
            // orders the window in regardless of activation state.
            macos::order_front_regardless(window);
            tracing::debug!(
                "window {i}: requested {:?}, actual pos {:?} size {:?} visible {:?}",
                region,
                window.outer_position(),
                window.inner_size(),
                window.is_visible(),
            );
            macos::debug_window_state(window, i);
        }
        // Hide the pooled surplus.
        for window in &self.windows[regions.len()..] {
            window.set_visible(false);
        }
        self.visible = regions.len();
        self.last_regions = regions;
    }

    fn apply_style(&mut self, style: CensorSettings) {
        if style == self.style {
            return;
        }
        self.style = style;
        let protected = self.content_protected();
        for (i, window) in self.windows.iter().enumerate() {
            window.set_content_protected(protected);
            macos::apply_style(window, &self.style);
            if let Some(region) = self.last_regions.get(i) {
                self.chromes[i].update(region, &self.style);
            }
        }
    }
}

impl ApplicationHandler<OverlayMsg> for OverlayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, msg: OverlayMsg) {
        match msg {
            OverlayMsg::Regions(regions) => self.apply_regions(event_loop, regions),
            OverlayMsg::Style(style) => self.apply_style(style),
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }
}

mod macos {
    use objc2::rc::Retained;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSColor, NSFont, NSTextAlignment, NSTextField, NSView, NSWindow,
        NSWindowCollectionBehavior,
    };
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_foundation::NSString;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    use super::CensorRegion;
    use crate::settings::{parse_color, CensorSettings};

    /// The text fields drawn on a censor window: a centered random text
    /// and a small trigger-class label near the bottom edge.
    pub struct Chrome {
        text: Option<Retained<NSTextField>>,
        label: Option<Retained<NSTextField>>,
    }

    impl Chrome {
        pub fn attach(window: &Window) -> Self {
            let empty = Self { text: None, label: None };
            let Some(mtm) = MainThreadMarker::new() else {
                return empty;
            };
            let Some(content) = ns_window(window).and_then(|ns| ns.contentView()) else {
                tracing::error!("no content view for overlay window");
                return empty;
            };
            let make = || {
                let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
                field.setAlignment(NSTextAlignment::Center);
                field.setHidden(true);
                content.addSubview(&field);
                field
            };
            Self {
                text: Some(make()),
                label: Some(make()),
            }
        }

        /// Set contents/fonts/frames for this box. Called whenever the
        /// window is (re)assigned a region or the style changes.
        pub fn update(&self, region: &CensorRegion, style: &CensorSettings) {
            let (w, h) = (region.width as f64, region.height as f64);
            let overlay = &style.text_overlay;
            let (r, g, b, a) = parse_color(&overlay.font_color).unwrap_or((1.0, 1.0, 1.0, 1.0));
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a);

            if let Some(text) = &self.text {
                let show = overlay.enabled && !overlay.lines.is_empty();
                text.setHidden(!show);
                if show {
                    let pick = pick_text(region, &overlay.lines);
                    text.setStringValue(&NSString::from_str(pick));
                    text.setTextColor(Some(&color));
                    text.setFont(Some(&font(&overlay.font_family, overlay.font_size_pt as f64)));
                    text.sizeToFit();
                    let size = text.frame().size;
                    text.setFrame(centered(size, w, (h - size.height) / 2.0));
                }
            }

            if let Some(label) = &self.label {
                label.setHidden(!style.show_trigger_label);
                if style.show_trigger_label {
                    label.setStringValue(&NSString::from_str(region.trigger));
                    label.setTextColor(Some(&color));
                    let size_pt = (overlay.font_size_pt as f64 * 0.6).clamp(9.0, 14.0);
                    label.setFont(Some(&font(&overlay.font_family, size_pt)));
                    label.sizeToFit();
                    let size = label.frame().size;
                    label.setFrame(centered(size, w, 4.0));
                }
            }
        }
    }

    fn centered(size: CGSize, container_width: f64, y: f64) -> CGRect {
        CGRect::new(
            CGPoint::new(((container_width - size.width) / 2.0).max(0.0), y.max(0.0)),
            size,
        )
    }

    fn font(family: &str, size: f64) -> Retained<NSFont> {
        NSFont::fontWithName_size(&NSString::from_str(family), size)
            .unwrap_or_else(|| NSFont::boldSystemFontOfSize(size))
    }

    /// The box's seed picks its text once, for its whole lifetime.
    fn pick_text<'a>(region: &CensorRegion, texts: &'a [String]) -> &'a str {
        &texts[(region.text_seed % texts.len() as u64) as usize]
    }

    fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        // SAFETY: winit guarantees the handle is a valid NSView pointer, and
        // we're on the main thread (this is only called from the event loop).
        let view = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
        view.window()
    }

    /// No shadow, and visible on every Space including fullscreen apps.
    /// (Click-through and capture exclusion are handled by winit's
    /// `set_cursor_hittest` / `set_content_protected`.)
    pub fn configure_censor_window(window: &Window) {
        let Some(ns) = ns_window(window) else {
            tracing::error!("could not get NSWindow for overlay window");
            return;
        };
        ns.setHasShadow(false);
        ns.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
    }

    pub fn order_front_regardless(window: &Window) {
        if let Some(ns) = ns_window(window) {
            ns.orderFrontRegardless();
        }
    }

    /// Log ground truth from the NSWindow itself (winit's bookkeeping can
    /// disagree with the window server).
    pub fn debug_window_state(window: &Window, i: usize) {
        let Some(ns) = ns_window(window) else {
            tracing::debug!("window {i}: no NSWindow");
            return;
        };
        let frame = ns.frame();
        tracing::debug!(
            "window {i} NSWindow: number={:?} visible={} sharing={:?} level={:?} alpha={} frame=({}, {}) {}x{} screenCount={}",
            ns.windowNumber(),
            ns.isVisible(),
            ns.sharingType(),
            ns.level(),
            ns.alphaValue(),
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
            objc2_app_kit::NSScreen::screens(objc2::MainThreadMarker::new().unwrap()).len(),
        );
    }

    /// Fill color on the window, border on the content view's layer.
    pub fn apply_style(window: &Window, style: &CensorSettings) {
        let Some(ns) = ns_window(window) else {
            return;
        };
        let (r, g, b, a) = parse_color(&style.fill_color).unwrap_or((0.0, 0.0, 0.0, 1.0));
        let fill = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a);
        ns.setBackgroundColor(Some(&fill));
        if let Some(view) = ns.contentView() {
            view.setWantsLayer(true);
            if let Some(layer) = view.layer() {
                let (r, g, b, a) =
                    parse_color(&style.border_color).unwrap_or((0.0, 0.0, 0.0, 1.0));
                let border = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a);
                layer.setBorderWidth(style.border_width as f64);
                let cg = border.CGColor();
                layer.setBorderColor(Some(cg.as_ref()));
            }
        }
    }
}
