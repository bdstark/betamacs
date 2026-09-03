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

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId, WindowLevel};

use crate::censor_fx::RegionContent;
use crate::settings::{CensorMode, CensorSettings};

/// A censor rectangle in global logical screen points (the same coordinate
/// space as ScreenCaptureKit display origins), plus optional processed
/// pixels for its interior (blur/mosaic modes).
#[derive(Debug, Clone)]
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
    /// Processed interior pixels (blur/mosaic); None for box/static modes.
    pub content: Option<RegionContent>,
    /// Debug highlight: Some(info label) renders this region as a
    /// transparent outlined annotation (flagged-but-not-blocked) instead
    /// of a censor box.
    pub highlight: Option<String>,
}

impl CensorRegion {
    pub fn same_geometry(&self, other: &Self) -> bool {
        self.x == other.x
            && self.y == other.y
            && self.width == other.width
            && self.height == other.height
    }
}

impl PartialEq for CensorRegion {
    fn eq(&self, other: &Self) -> bool {
        self.same_geometry(other)
            && self.trigger == other.trigger
            && self.text_seed == other.text_seed
            && self.highlight == other.highlight
            && match (&self.content, &other.content) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(&a.rgba, &b.rgba),
                _ => false,
            }
    }
}

#[derive(Debug)]
pub enum OverlayMsg {
    Regions(Vec<CensorRegion>),
    Style(CensorSettings),
    /// Status text for the menu bar's "monitoring" line.
    Status(String),
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

    /// Update the menu bar's "monitoring" status line.
    pub fn set_status(&self, text: String) -> Result<()> {
        self.proxy
            .send_event(OverlayMsg::Status(text))
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
    /// Next TV-static animation frame, when mode = static and animated.
    next_noise_tick: Option<Instant>,
    /// xorshift state for noise generation.
    rng: u64,
    /// Menu bar status item, when installed (run mode only).
    menubar: Option<crate::menubar::MenuBar>,
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
                next_noise_tick: None,
                rng: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9e3779b97f4a7c15)
                    | 1,
                menubar: None,
            },
        ))
    }

    /// Install the menu bar status item (main thread, run mode only).
    pub fn set_menubar(&mut self, menubar: crate::menubar::MenuBar) {
        self.menubar = Some(menubar);
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
            let geometry_changed = self
                .last_regions
                .get(i)
                .is_none_or(|prev| !prev.same_geometry(region));
            if geometry_changed {
                let window = &self.windows[i];
                let _ = window.request_inner_size(LogicalSize::new(region.width, region.height));
                window.set_outer_position(LogicalPosition::new(region.x, region.y));
            }
            // Pooled windows switch roles (censor <-> highlight), so the
            // per-role styling is applied on every assignment.
            let role_changed = self
                .last_regions
                .get(i)
                .is_none_or(|prev| prev.highlight.is_some() != region.highlight.is_some());
            if role_changed {
                if region.highlight.is_some() {
                    macos::apply_highlight_style(&self.windows[i]);
                } else {
                    macos::apply_style(&self.windows[i], &self.style);
                }
            }
            self.chromes[i].update(region, &self.style);
            self.render_interior(i, region);
            let window = &self.windows[i];
            window.set_visible(true);
            // winit's set_visible uses orderFront, which the window server
            // can ignore for a background (never-activated) app; this
            // orders the window in regardless of activation state.
            macos::order_front_regardless(window);
            macos::debug_window_state(window, i);
        }
        // Hide the pooled surplus.
        for window in &self.windows[regions.len()..] {
            window.set_visible(false);
        }
        self.visible = regions.len();
        self.last_regions = regions;
        if let Some(mb) = &self.menubar {
            mb.set_boxes(self.visible);
        }
        self.schedule_noise();
    }

    /// Draw one region's interior according to the censor mode.
    fn render_interior(&mut self, i: usize, region: &CensorRegion) {
        let window = &self.windows[i];
        if region.highlight.is_some() {
            macos::clear_layer_contents(window);
            return;
        }
        match self.style.mode {
            CensorMode::Box => macos::clear_layer_contents(window),
            CensorMode::Blur | CensorMode::Mosaic => {
                if let Some(content) = &region.content {
                    macos::set_layer_contents(window, content);
                }
            }
            CensorMode::Static => {
                let content = macos::make_noise(
                    window,
                    region.width,
                    region.height,
                    &self.style.static_noise,
                    &mut self.rng,
                );
                macos::set_layer_contents(window, &content);
            }
        }
    }

    /// (Re)arm the static-noise animation timer.
    fn schedule_noise(&mut self) {
        let animate = self.style.mode == CensorMode::Static
            && self.visible > 0
            && self.style.static_noise.speed_hz > 0.0;
        self.next_noise_tick = animate.then(|| {
            Instant::now()
                + Duration::from_secs_f32(1.0 / self.style.static_noise.speed_hz.clamp(0.2, 30.0))
        });
    }

    fn noise_tick(&mut self) {
        for i in 0..self.visible {
            if let Some(region) = self.last_regions.get(i).cloned() {
                self.render_interior(i, &region);
            }
        }
        self.schedule_noise();
    }

    fn apply_style(&mut self, style: CensorSettings) {
        if style == self.style {
            return;
        }
        self.style = style;
        let protected = self.content_protected();
        for i in 0..self.windows.len() {
            let window = &self.windows[i];
            window.set_content_protected(protected);
            // Don't clobber windows currently serving as highlights.
            let is_highlight = self
                .last_regions
                .get(i)
                .is_some_and(|r| r.highlight.is_some());
            if is_highlight {
                macos::apply_highlight_style(window);
            } else {
                macos::apply_style(window, &self.style);
            }
            if let Some(region) = self.last_regions.get(i).cloned() {
                self.chromes[i].update(&region, &self.style);
                self.render_interior(i, &region);
            }
        }
        self.schedule_noise();
    }
}

impl ApplicationHandler<OverlayMsg> for OverlayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, msg: OverlayMsg) {
        match msg {
            OverlayMsg::Regions(regions) => self.apply_regions(event_loop, regions),
            OverlayMsg::Style(style) => self.apply_style(style),
            OverlayMsg::Status(text) => {
                if let Some(mb) = &self.menubar {
                    mb.set_status(&text);
                }
            }
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.next_noise_tick {
            Some(due) => {
                if Instant::now() >= due {
                    self.noise_tick();
                }
                if let Some(next) = self.next_noise_tick {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(next));
                }
            }
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
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

    use objc2_core_foundation::CFRetained;

    use super::CensorRegion;
    use crate::censor_fx::RegionContent;
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
            if let Some(info) = &region.highlight {
                let color = highlight_color();
                if let Some(text) = &self.text {
                    text.setHidden(false);
                    text.setStringValue(&NSString::from_str(info));
                    text.setTextColor(Some(&color));
                    text.setFont(Some(&NSFont::boldSystemFontOfSize(11.0)));
                    text.sizeToFit();
                    let size = text.frame().size;
                    // Top-left corner, just inside the outline.
                    text.setFrame(CGRect::new(
                        CGPoint::new(3.0, (region.height as f64 - size.height - 3.0).max(0.0)),
                        size,
                    ));
                }
                if let Some(label) = &self.label {
                    label.setHidden(true);
                }
                return;
            }
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

    pub fn highlight_color() -> Retained<NSColor> {
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.62, 0.04, 1.0)
    }

    /// Transparent, outlined, labeled: the flagged-but-not-blocked
    /// debug annotation look.
    pub fn apply_highlight_style(window: &Window) {
        let Some(ns) = ns_window(window) else {
            return;
        };
        ns.setOpaque(false);
        ns.setBackgroundColor(Some(&NSColor::clearColor()));
        ns.setAlphaValue(1.0);
        if let Some(view) = ns.contentView() {
            view.setWantsLayer(true);
            if let Some(layer) = view.layer() {
                layer.setBorderWidth(2.0);
                let cg = highlight_color().CGColor();
                layer.setBorderColor(Some(cg.as_ref()));
            }
        }
    }

    /// Fill color, opacity, and border on the content view's layer.
    pub fn apply_style(window: &Window, style: &CensorSettings) {
        let Some(ns) = ns_window(window) else {
            return;
        };
        let (r, g, b, a) = parse_color(&style.fill_color).unwrap_or((0.0, 0.0, 0.0, 1.0));
        let fill = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a);
        ns.setBackgroundColor(Some(&fill));
        ns.setAlphaValue((style.opacity_pct as f64 / 100.0).clamp(0.1, 1.0));
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

    fn content_layer(window: &Window) -> Option<Retained<objc2_quartz_core::CALayer>> {
        let view = ns_window(window)?.contentView()?;
        view.setWantsLayer(true);
        view.layer()
    }

    /// Display processed RGBA pixels as the window's interior. Mosaic and
    /// noise images are cell-resolution; nearest-neighbor magnification
    /// keeps them crisp-blocky on the GPU.
    pub fn set_layer_contents(window: &Window, content: &RegionContent) {
        let Some(layer) = content_layer(window) else {
            return;
        };
        let Some(image) = cg_image_from_rgba(content.width, content.height, &content.rgba) else {
            tracing::warn!("could not build CGImage for censor content");
            return;
        };
        unsafe {
            layer.setMagnificationFilter(if content.pixelated {
                objc2_quartz_core::kCAFilterNearest
            } else {
                objc2_quartz_core::kCAFilterLinear
            });
            // CALayer.contents takes a CGImage through the id-typed API.
            let obj: &objc2::runtime::AnyObject =
                &*(CFRetained::as_ptr(&image).as_ptr() as *const objc2::runtime::AnyObject);
            layer.setContents(Some(obj));
        }
    }

    /// Back to plain fill color (box mode).
    pub fn clear_layer_contents(window: &Window) {
        if let Some(layer) = content_layer(window) {
            unsafe { layer.setContents(None) };
        }
    }

    /// Points per millimeter of the window's display, from the display's
    /// reported physical size; falls back to a typical desktop value.
    fn points_per_mm(window: &Window) -> f32 {
        let fallback = 6.0;
        let Some(screen) = ns_window(window).and_then(|ns| ns.screen()) else {
            return fallback;
        };
        let number = screen
            .deviceDescription()
            .objectForKey(&*objc2_foundation::NSString::from_str("NSScreenNumber"));
        let Some(number) = number else { return fallback };
        let display_id: u32 = unsafe {
            objc2::msg_send![&*number, unsignedIntValue]
        };
        let size_mm = objc2_core_graphics::CGDisplayScreenSize(display_id);
        let width_pt = screen.frame().size.width;
        if size_mm.width > 1.0 && width_pt > 1.0 {
            (width_pt / size_mm.width) as f32
        } else {
            fallback
        }
    }

    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    /// Generate one frame of analog-TV static at grain resolution.
    pub fn make_noise(
        window: &Window,
        width_pt: f32,
        height_pt: f32,
        settings: &crate::settings::StaticSettings,
        rng: &mut u64,
    ) -> RegionContent {
        let grain_pt = (settings.grain_mm.clamp(0.2, 10.0) * points_per_mm(window)).max(1.0);
        let cells_w = ((width_pt / grain_pt).ceil() as u32).max(1);
        let cells_h = ((height_pt / grain_pt).ceil() as u32).max(1);
        let density = (settings.density_pct / 100.0).clamp(0.0, 1.0);
        let low = parse_color(&settings.color_low).unwrap_or((0.0, 0.0, 0.0, 1.0));
        let high = parse_color(&settings.color_high).unwrap_or((1.0, 1.0, 1.0, 1.0));
        let mut rgba = vec![0u8; (cells_w * cells_h * 4) as usize];
        for cell in rgba.chunks_exact_mut(4) {
            let roll = xorshift(rng);
            let lit = (roll & 0xffff) as f32 / 65535.0 < density;
            let t = ((roll >> 16) & 0xffff) as f32 / 65535.0;
            let (r, g, b) = if !lit {
                (0.0, 0.0, 0.0)
            } else if settings.colored {
                (
                    low.0 as f32 + (high.0 as f32 - low.0 as f32) * t,
                    low.1 as f32 + (high.1 as f32 - low.1 as f32) * t,
                    low.2 as f32 + (high.2 as f32 - low.2 as f32) * t,
                )
            } else {
                (t, t, t)
            };
            cell[0] = (r * 255.0) as u8;
            cell[1] = (g * 255.0) as u8;
            cell[2] = (b * 255.0) as u8;
            cell[3] = 255;
        }
        RegionContent {
            width: cells_w,
            height: cells_h,
            rgba: std::sync::Arc::new(rgba),
            pixelated: true,
        }
    }

    fn cg_image_from_rgba(
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Option<CFRetained<objc2_core_graphics::CGImage>> {
        use objc2_core_graphics::{CGBitmapInfo, CGColorRenderingIntent, CGDataProvider, CGImage};
        let data = objc2_core_foundation::CFData::from_bytes(rgba);
        let provider = CGDataProvider::with_cf_data(Some(&data))?;
        let space = objc2_core_graphics::CGColorSpace::new_device_rgb()?;
        unsafe {
            CGImage::new(
                width as usize,
                height as usize,
                8,
                32,
                width as usize * 4,
                Some(&space),
                CGBitmapInfo(objc2_core_graphics::CGImageAlphaInfo::NoneSkipLast.0),
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
    }
}
