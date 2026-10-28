#![cfg_attr(not(test), windows_subsystem = "windows")]
mod contact_backend;
mod diagnostics;
mod display_state;
mod settings_store;
mod startup;
mod tray;
mod notify;
mod graphics;
#[cfg(test)]
mod screenshots;
mod radio_backend;
mod stream_backend;
mod source_backend;
#[cfg(test)]
mod phone_checks;
mod ui;

use std::{
    error::Error,
    path::PathBuf,
    time::{Duration, Instant},
};
use windows::Win32::Foundation::HWND;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    window::{Window, WindowId},
};

type AnyResult<T> = Result<T, Box<dyn Error>>;
#[derive(Debug)]
enum Event {
    Bluetooth(meshlive::bluetooth::Capabilities),
    Access(egui_winit::accesskit_winit::Event),
    Repaint { when: Instant, pass: u64 },
    Tray(tray::TrayEvent),
}
impl From<egui_winit::accesskit_winit::Event> for Event {
    fn from(e: egui_winit::accesskit_winit::Event) -> Self {
        Self::Access(e)
    }
}

#[derive(Default)]
struct Options {
    warp: bool,
    level10: bool,
    texture: bool,
    demo: bool,
    bluetooth: bool,
    exit: Option<Duration>,
    metrics: Option<PathBuf>,
    /// Started at sign-in (Settings > Start with Windows): the window stays hidden in the tray.
    background: bool,
}
impl Options {
    fn parse() -> AnyResult<Self> {
        // The product runs the Bluetooth owner by default (a packaged
        // app has no arguments); `--offline` is the UI without the radio.
        let mut value = Self { bluetooth: true, ..Self::default() };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--warp" => value.warp = true,
                "--feature-level-10" => value.level10 = true,
                "--texture" => value.texture = true,
                // Experimental: scripted sample people and streams (no radio or media).
                "--demo" => value.demo = true,
                // Experimental: the real Bluetooth owner (outgoing calls only);
                // the radio is used only when the user searches or calls.
                "--bluetooth" => value.bluetooth = true,
                "--offline" => value.bluetooth = false,
                "--background" => value.background = true,
                "--exit-after" => {
                    let seconds: u64 = args.next().ok_or("Missing exit seconds")?.parse()?;
                    if !(1..=600).contains(&seconds) {
                        return Err("Exit duration outside 1–600 seconds".into());
                    }
                    value.exit = Some(Duration::from_secs(seconds));
                }
                "--metrics" => {
                    value.metrics = Some(args.next().ok_or("Missing metrics path")?.into())
                }
                _ => return Err(format!("Unknown option: {arg}").into()),
            }
        }
        Ok(value)
    }
}

fn icon_pixels() -> AnyResult<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../assets/icon.png")));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut bytes = vec![0; reader.output_buffer_size().ok_or("Icon too large")?];
    let info = reader.next_frame(&mut bytes)?;
    bytes.truncate(info.buffer_size());
    let pixels = match info.color_type {
        png::ColorType::Rgba => bytes,
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255])
            .collect(),
        _ => return Err("Application icon must be RGB or RGBA".into()),
    };
    Ok((pixels, info.width, info.height))
}

struct View {
    window: Window,
    graphics: graphics::Graphics,
    ctx: egui::Context,
    input: egui_winit::State,
    ui: ui::Ui,
}
struct App {
    options: Options,
    proxy: EventLoopProxy<Event>,
    view: Option<View>,
    repaint: Option<Instant>,
    started: Instant,
    frames: Vec<f64>,
    error: Option<String>,
    /// UI-loop health during a call: the last painted frame, the window's
    /// occlusion and focus as winit reported them, and the model ticks run
    /// without painting (minimised, occluded or throttled window).
    last_draw: Option<Instant>,
    occluded: Option<bool>,
    focused: bool,
    last_background: Instant,
    background_ticks: u32,
    /// The notification-area icon (while hidden there, or after the first hide).
    tray: Option<tray::Tray>,
    /// Start hidden: launched by Windows at sign-in.
    start_hidden: bool,
}
impl App {
    /// The tray icon, created once (its events come back as `Event::Tray`).
    fn ensure_tray(&mut self) {
        if self.tray.is_none() {
            let proxy = self.proxy.clone();
            self.tray = tray::Tray::start(move |event| {
                let _ = proxy.send_event(Event::Tray(event));
            });
            radio_backend::trace("tray", if self.tray.is_some() { "added" } else { "failed" });
        }
    }
    /// Close while calls are allowed: hide to the tray (first time: say so).
    fn hide_to_tray(&mut self) {
        self.ensure_tray();
        let Some(view) = self.view.as_mut() else { return };
        if self.tray.is_none() {
            // No tray icon (Explorer not running): never strand a hidden window.
            view.window.set_minimized(true);
            return;
        }
        view.window.set_visible(false);
        radio_backend::trace("window", "hidden_to_tray");
        if !view.ui.switches.tray_hint_shown {
            if let Some(tray) = &self.tray {
                tray.balloon(tray::HINT_TITLE, tray::HINT_TEXT);
            }
            view.ui.switches.tray_hint_shown = true;
            view.ui.switches_changed();
        }
    }
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: impl std::fmt::Display) {
        self.error = Some(error.to_string());
        event_loop.exit();
    }
    fn create(&mut self, event_loop: &ActiveEventLoop) -> AnyResult<View> {
        // AccessKit must be installed before the native window is first shown.
        let (pixels, width, height) = icon_pixels()?;
        let icon = winit::window::Icon::from_rgba(pixels, width, height)?;
        use winit::platform::windows::{Color, WindowAttributesExtWindows};
        // A black caption bar with white text, like the app (Windows 11
        // colours; the dark theme gives Windows 10 its dark caption).
        let window = event_loop.create_window(
            WindowAttributesExtWindows::with_class_name(Window::default_attributes(), meshlive::app_mode::current().window_class())
                .with_theme(Some(winit::window::Theme::Dark))
                .with_title_background_color(Some(Color::from_rgb(0, 0, 0)))
                .with_title_text_color(Color::from_rgb(255, 255, 255))
                .with_title(meshlive::app_mode::current().window_title())
                .with_window_icon(Some(icon))
                .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 720.0))
                .with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 480.0))
                .with_visible(false),
        )?;
        let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
            return Err("Expected native Windows handle".into());
        };
        let size = window.inner_size();
        let graphics = graphics::Graphics::new(
            HWND(handle.hwnd.get() as _),
            size.width,
            size.height,
            self.options.warp,
            self.options.level10,
        )?;
        let ctx = egui::Context::default();
        ui::configure(&ctx);
        let proxy = self.proxy.clone();
        ctx.set_request_repaint_callback(move |request| {
            if let Some(when) = Instant::now().checked_add(request.delay) {
                let _ = proxy.send_event(Event::Repaint {
                    when,
                    pass: request.current_cumulative_pass_nr,
                });
            }
        });
        let mut input = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(8192),
        );
        input.init_accesskit(event_loop, &window, self.proxy.clone());
        // CF-PRIV-26: an interrupted wipe finishes before anything opens a store.
        let app_dir = meshlive::sealed::default_dir();
        let (resumed_wipe, opened) = meshlive::wipe::resume_before_open(app_dir.as_deref(),
            &mut || meshlive::identity::Identity::delete_persisted().map_err(|e| format!("{e:?}")), || {
                let backend: Box<dyn ui::backend::Backend> = if self.options.bluetooth && !self.options.demo {
                    Box::new(radio_backend::RadioBackend::start())
                } else { Box::new(ui::backend::OfflineBackend::default()) };
                backend
            });
        let blocked = opened.is_none();
        let inner = opened.unwrap_or_else(|| Box::new(ui::backend::OfflineBackend::default()));
        // Verified contacts come from the DPAPI-sealed store in the app folder.
        let dir = app_dir.clone().filter(|_| !blocked);
        let mut contacts = contact_backend::ContactBackend::new(
            inner,
            dir.as_deref(),
            match dir.clone() {
                // Forget/Reset also revoke leases and drop claims (Android forget).
                Some(dir) => Box::new(contact_backend::AppHooks(dir)) as Box<dyn contact_backend::ContactHooks>,
                None => Box::new(contact_backend::NoHooks),
            },
        );
        if let Some(dir) = dir.clone() {
            contacts = contacts.with_profile(meshlive::profile::Profile::open_default(&dir));
            contacts = contacts.with_settings(settings_store::SettingsStore::open_default(&dir));
            contacts = contacts.with_app_dir(dir.clone());
            // "Your ID" as Android shows it (the identity fingerprint's handle).
            let own_id = meshlive::callee::node_id().map(|node| meshlive::names::handle(&node[..6].try_into().unwrap()));
            contacts = contacts.with_own_id(own_id);
            contacts = contacts.with_blocks(meshlive::blocking::BlockList::open_default(&dir));
        }
        // A blocked launch keeps only the explicit wipe retry; no stores open.
        if let Some(dir) = app_dir { contacts = contacts.with_wipe(Box::new(move || meshlive::wipe::run_installed(&dir))); }
        let mut ui = ui::Ui::with_backend(Box::new(contacts));
        // Saved settings replace the defaults and reach the Bluetooth owner at once.
        if let Some(saved) = dir.as_deref().and_then(|d| settings_store::SettingsStore::open_default(d).load()) {
            ui.switches = saved;
            ui.switches_changed();
        }
        if let Some(report) = resumed_wipe {
            ui.apply(ui::backend::Update::Wiped(report));
        }
        ui.texture_visible = self.options.texture;
        ui.set_demo(self.options.demo);
        if self.start_hidden {
            radio_backend::trace("start_hidden", "sign_in");
        } else {
            window.set_visible(true);
        }
        // Test-only: start minimised, to check the toast and flash headlessly.
        #[cfg(feature = "test-hooks")]
        if std::env::var("MESHLIVE_TEST_MINIMIZE").ok().as_deref() == Some("1") {
            window.set_minimized(true);
            radio_backend::trace("test_hook", "minimised at start");
        }
        window.request_redraw();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = proxy.send_event(Event::Bluetooth(meshlive::bluetooth::probe()));
        });
        Ok(View {
            window,
            graphics,
            ctx,
            input,
            ui,
        })
    }
    fn draw(&mut self) -> AnyResult<()> {
        let Some(view) = self.view.as_mut() else {
            return Ok(());
        };
        if view.window.is_minimized() == Some(true) {
            self.repaint = None;
            return Ok(());
        }
        let started = Instant::now();
        if let Some(previous) = self.last_draw {
            let interval = started - previous;
            if interval >= Duration::from_millis(100) && view.ui.in_call() {
                radio_backend::trace(
                    "ui_slow",
                    &format!(
                        "interval_ms={} minimized={:?} occluded={:?} focused={} display={} input_desktop={} background_ticks={}",
                        interval.as_millis(),
                        view.window.is_minimized(),
                        self.occluded,
                        self.focused,
                        display_state::display(),
                        display_state::input_desktop(),
                        self.background_ticks
                    ),
                );
            }
        }
        self.last_draw = Some(started);
        self.background_ticks = 0;
        let input = view.input.take_egui_input(&view.window);
        #[cfg(test)]
        let input = {
            let mut input = input;
            let events = view.ui.native_check_input(&input);
            input.events.extend(events);
            input
        };
        let description = format!(
            "Direct3D 11 · {} · feature level {:#x}",
            if view.graphics.warp {
                "WARP"
            } else {
                "hardware"
            },
            view.graphics.level.0
        );
        let output = view.ctx.run(input, |ctx| {
            view.ui.show(ctx, view.graphics.texture_id, &description)
        });
        announce_incoming(view);
        let delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX);
        let (render, platform, _) = egui_directx11::split_output(output);
        view.input.handle_platform_output(&view.window, platform);
        view.graphics.render(&view.ctx, render)?;
        // This includes Present wait. It is not a GPU completion or physical presentation timestamp.
        if self.options.metrics.is_some() && self.frames.len() < 65536 {
            self.frames.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        self.repaint = if delay == Duration::MAX {
            None
        } else {
            Instant::now().checked_add(delay)
        };
        Ok(())
    }
    fn metrics(&self) -> AnyResult<()> {
        let Some(path) = &self.options.metrics else {
            return Ok(());
        };
        let warp = self.view.as_ref().is_some_and(|v| v.graphics.warp);
        let level = self.view.as_ref().map(|v| v.graphics.level.0).unwrap_or(0);
        let times = self
            .frames
            .iter()
            .map(|v| format!("{v:.4}"))
            .collect::<Vec<_>>()
            .join(",");
        let contents = format!(
            "{{\"scope\":\"native-ui-synthetic-texture-only\",\"elapsed_seconds\":{:.4},\"frames\":{},\"warp\":{},\"feature_level\":{},\"frame_cpu_and_present_wait_ms\":[{}],\"failed\":{}}}\n",
            self.started.elapsed().as_secs_f64(),
            self.frames.len(),
            warp,
            level,
            times,
            self.error.is_some()
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(())
    }
}
impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.view.is_none() {
            match self.create(event_loop) {
                Ok(view) => self.view = Some(view),
                Err(e) => self.fail(event_loop, e),
            }
            if self.start_hidden && self.view.is_some() {
                self.ensure_tray();
                if self.tray.is_none() {
                    if let Some(view) = &self.view {
                        view.window.set_visible(true);
                    }
                }
            }
        }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Tray(tray::TrayEvent::Open) => {
                if let Some(view) = self.view.as_mut() {
                    view.window.set_visible(true);
                    view.window.set_minimized(false);
                    view.window.focus_window();
                    view.window.request_redraw();
                }
            }
            Event::Tray(tray::TrayEvent::Exit) => event_loop.exit(),
            Event::Bluetooth(capabilities) => {
                if let Some(view) = self.view.as_mut() {
                    view.ui.bluetooth = Some(capabilities);
                    view.window.request_redraw();
                }
            }
            Event::Repaint { when, pass } => {
                if self
                    .view
                    .as_ref()
                    .is_some_and(|v| v.ctx.cumulative_pass_nr() <= pass.saturating_add(1))
                {
                    self.repaint = Some(self.repaint.map_or(when, |prior| prior.min(when)));
                }
            }
            Event::Access(e) => {
                if let Some(view) = self.view.as_mut() {
                    if e.window_id == view.window.id() {
                        match e.window_event {
                            egui_winit::accesskit_winit::WindowEvent::InitialTreeRequested => {
                                view.ctx.enable_accesskit()
                            }
                            egui_winit::accesskit_winit::WindowEvent::ActionRequested(request) => {
                                view.input.on_accesskit_action_request(request)
                            }
                            egui_winit::accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                                view.ctx.disable_accesskit()
                            }
                        }
                        view.window.request_redraw();
                    }
                }
            }
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if id != view.window.id() {
            return;
        }
        let response = view.input.on_window_event(&view.window, &event);
        if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            view.window.request_redraw();
        }
        // OS lifecycle/render events must be processed even when egui consumes keyboard/pointer input.
        match event {
            // Closing must not stop calls: while calls are allowed it hides to the tray.
            WindowEvent::CloseRequested => match tray::close_action(view.ui.calls_allowed()) {
                tray::CloseAction::Exit => event_loop.exit(),
                tray::CloseAction::HideToTray => self.hide_to_tray(),
            },
            WindowEvent::Occluded(occluded) => self.occluded = Some(occluded),
            WindowEvent::Focused(focused) => self.focused = focused,
            WindowEvent::Resized(size) => {
                if let Err(e) = view.graphics.resize(size.width, size.height) {
                    self.fail(event_loop, e)
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.draw() {
                    self.fail(event_loop, e)
                }
            }
            _ => {}
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let exit = self.options.exit.map(|duration| self.started + duration);
        if exit.is_some_and(|deadline| now >= deadline) {
            event_loop.exit();
            return;
        }
        if self.repaint.is_some_and(|deadline| now >= deadline) {
            self.repaint = None;
            if let Some(view) = &self.view {
                view.window.request_redraw();
            }
        }
        // During a call the model (backend updates, call signals, test hooks)
        // runs even when Windows does not paint the window: a minimised,
        // occluded or throttled window must not stall the call's UI side.
        let mut background = None;
        if let Some(view) = self.view.as_mut() {
            #[cfg(test)]
            if view.ui.native_check_done() { event_loop.exit(); return; }
            // Hidden in the tray: no painting, so the model ticks here (an
            // incoming call shows the window again, see `announce_incoming`).
            let hidden = view.window.is_visible() == Some(false);
            if hidden && !view.ui.in_call() {
                if now - self.last_background >= Duration::from_millis(200) {
                    self.last_background = now;
                    view.ui.background_tick();
                    announce_incoming(view);
                }
                background = Some(now + Duration::from_millis(200));
            }
            if view.ui.in_call() {
                let painting = self.last_draw.is_some_and(|at| now - at < Duration::from_millis(100));
                if !painting && now - self.last_background >= Duration::from_millis(50) {
                    self.last_background = now;
                    self.background_ticks += 1;
                    view.ui.background_tick();
                    announce_incoming(view);
                }
                background = Some(now + Duration::from_millis(50));
            }
            #[cfg(test)]
            if view.ui.native_check_armed() {
                view.ui.background_tick();
                if view.ui.native_check_take_focus_request() { view.window.focus_window(); }
                announce_incoming(view);
                view.window.request_redraw();
                background = Some(now + Duration::from_millis(50));
            }
        }
        let repaint = match (self.repaint, background) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, None) => a,
            (None, b) => b,
        };
        let deadline = match (repaint, exit) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, None) => a,
            (None, b) => b,
        };
        event_loop.set_control_flow(deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}
/// A ringing incoming call: flash the taskbar until the window is in front
/// and show a toast when the window is not focused.
fn announce_incoming(view: &mut View) {
    if let Some((peer, video)) = view.ui.take_incoming_alert() {
        // Hidden in the tray: a ringing call brings the window back.
        if view.window.is_visible() == Some(false) {
            view.window.set_visible(true);
            view.window.request_redraw();
        }
        view.window.request_user_attention(Some(winit::window::UserAttentionType::Critical));
        if !view.window.has_focus() {
            match notify::incoming_call(&peer, video) {
                Ok(()) => radio_backend::trace("toast_shown", &format!("attention={:?}", notify::attention())),
                Err(error) => radio_backend::trace("toast_failed", &error),
            }
        }
    }
}

/// One MeshLive window per user session: a second start (a second click, or a
/// toast click on a packaged app) brings the running window to the front and
/// exits. Tools (call_phone and the rest) are separate programs.
fn activate_running_instance() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow};
    let class: Vec<u16> = meshlive::app_mode::current().window_class().encode_utf16().chain(Some(0)).collect();
    let Ok(window) = (unsafe { FindWindowW(windows::core::PCWSTR(class.as_ptr()), windows::core::PCWSTR::null()) }) else {
        return false;
    };
    if window.is_invalid() {
        return false;
    }
    unsafe {
        if !windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(window).as_bool() {
            // Hidden in the tray: a second start opens it.
            let _ = ShowWindow(window, windows::Win32::UI::WindowsAndMessaging::SW_SHOW);
        }
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        let _ = SetForegroundWindow(window);
    }
    true
}

/// Running from an MSIX package (its install folder holds AppxManifest.xml):
/// the package identity names the app, so no explicit AppUserModelID is set
/// (one would break the packaged app's toasts and taskbar identity).
pub(crate) fn packaged() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("AppxManifest.xml").is_file()))
        .unwrap_or(false)
}

fn main() -> AnyResult<()> {
    if activate_running_instance() {
        return Ok(());
    }
    // Crash reports: written locally only, offered on the next launch.
    if let Some(dir) = meshlive::sealed::default_dir() {
        meshlive::crash::install(dir, env!("CARGO_PKG_VERSION"));
    }
    let _mta = meshlive::bluetooth::MtaUsage::for_process()
        .map_err(|error| format!("Couldn't initialize Windows services ({error:?})."))?;
    // Test-hooks only: `MESHLIVE_TEST_CRASH=panic|native` crashes at start,
    // to check the crash report (panic message carries a fake key and name).
    #[cfg(feature = "test-hooks")]
    match std::env::var("MESHLIVE_TEST_CRASH").ok().as_deref() {
        Some("panic") => panic!("test crash for \"Asha\" with key 00112233445566778899aabbccddeeff"),
        Some("native") => unsafe {
            std::ptr::write_volatile(std::ptr::null_mut::<u8>().wrapping_add(8), 1);
        },
        _ => {}
    }
    if !packaged() && !meshlive::app_mode::current().is_review() {
        unsafe {
            windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::w!(
                "io.github.meshlive.app"
            ))?;
        }
        // Toasts from an unpackaged release need a Start-menu shortcut with
        // this app ID (created once). Test-hooks builds create none (their
        // incoming call keeps the in-app ring and the taskbar flash).
        notify::set_unpackaged(true);
        #[cfg(not(feature = "test-hooks"))]
        let _ = notify::ensure_start_shortcut();
    }
    let options = Options::parse()?;
    let event_loop = EventLoop::<Event>::with_user_event().build()?;
    let mut app = App {
        options,
        proxy: event_loop.create_proxy(),
        view: None,
        repaint: None,
        started: Instant::now(),
        frames: Vec::new(),
        error: None,
        last_draw: None,
        occluded: None,
        focused: true,
        last_background: Instant::now(),
        background_ticks: 0,
        tray: None,
        start_hidden: false,
    };
    app.start_hidden = startup::launched_at_sign_in(app.options.background, packaged());
    display_state::start();
    event_loop.run_app(&mut app)?;
    app.metrics()?;
    if let Some(error) = app.error {
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod startup_tests {
    #[test]
    fn original_rgb_icon_loads_as_an_opaque_native_icon() {
        let (rgba, width, height) = super::icon_pixels().unwrap();
        assert_eq!((width, height), (512, 512));
        assert_eq!(rgba.len(), 512 * 512 * 4);
        assert!(rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));
        winit::window::Icon::from_rgba(rgba, width, height).unwrap();
    }
}
