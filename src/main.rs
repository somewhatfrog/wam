mod monitor;
mod prompt;
mod surface;
mod text;

use std::error::Error;
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Surface};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::foreign_toplevel_list::{ForeignToplevelList, ForeignToplevelListHandler};

use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, KeyboardHandler};

use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};


use wayland_client::protocol::{
    wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1,
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1,
    ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
    ext_image_copy_capture_manager_v1,
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1,
    ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
};

use monitor::{FrameAction, Monitor, SessionAction};
use prompt::{AppEntry, Prompt};
use text::Font;

const APP_ID: &str = "wam";

fn output_dpi(mm: (i32, i32), px: (i32, i32)) -> f32 {
    let (mw, mh) = mm;
    let (pw, ph) = px;
    if mw <= 0 || mh <= 0 || pw <= 0 || ph <= 0 {
        return 96.0;
    }
    let px_diag = (f64::from(pw).powi(2) + f64::from(ph).powi(2)).sqrt();
    let inch = (f64::from(mw).powi(2) + f64::from(mh).powi(2)).sqrt() * 0.039_370_08;
    let dpi = px_diag / inch;
    if dpi <= 1000.0 {
        dpi as f32
    } else {
        96.0
    }
}

struct Wam {
    registry_state: RegistryState,
    compositor_state: CompositorState,
    xdg_shell: XdgShell,
    shm: Shm,
    seat_state: SeatState,
    output_state: OutputState,
    foreign_toplevel_list: ForeignToplevelList,

    capture_source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,

    pool: SlotPool,
    font: Font,
    dpi: f32,
    window_output: Option<wl_output::WlOutput>,

    prompt: Option<Prompt>,
    monitor: Option<Monitor>,
    uncapped: bool,
    next_capture: Option<Instant>,

    quit: bool,
}

const HELP: &str = "\
wam - wayland app monitor

Usage:
  wam [OPTIONS]

Options:
  -h, --help            Print this help and exit
  -v, --version         Print version and exit
  -f, --font SPEC       Font to use: [name] [size]
                        (default: system's monospace font)
  -u, --uncapped        Capture at the compositor's full rate
                        (default: half the monitor's refresh rate)
";

fn arg_value(flag: &str, args: &mut std::iter::Peekable<impl Iterator<Item = String>>) -> String {
    match args.peek() {
        Some(v) if !v.starts_with('-') => args.next().unwrap(),
        _ => {
            eprintln!("wam: {flag} requires a value\n");
            eprint!("{HELP}");
            std::process::exit(1);
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut font_spec = String::new();
    let mut uncapped = false;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "-v" | "--version" => {
                println!("wam {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-f" | "--font" => {
                font_spec = arg_value("--font", &mut args);
            }
            "-u" | "--uncapped" => {
                uncapped = true;
            }
            other => {
                eprintln!("wam: unknown argument: {other}\n");
                eprint!("{HELP}");
                std::process::exit(1);
            }
        }
    }

    let font = match Font::from_spec(&font_spec) {
        Ok(font) => font,
        Err(e) => {
            eprintln!("wam: {e}");
            std::process::exit(1);
        }
    };

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = wayland_client::globals::registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let registry_state = RegistryState::new(&globals);
    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let foreign_toplevel_list = ForeignToplevelList::new(&globals, &qh);

    let capture_source_manager = globals.bind(&qh, 1..=1, ()).ok();
    let capture_manager = globals.bind(&qh, 1..=1, ()).ok();
    if capture_source_manager.is_none() || capture_manager.is_none() {
        eprintln!(
            "wam: ext-image-copy-capture protocol not available; \
             monitoring will not work on this compositor"
        );
    }

    let pool = SlotPool::new(1024 * 1024, &shm)?;

    let mut wam = Wam {
        registry_state,
        compositor_state,
        xdg_shell,
        shm,
        seat_state,
        output_state,
        foreign_toplevel_list,
        capture_source_manager,
        capture_manager,
        pool,
        font,
        dpi: 96.0,
        window_output: None,
        prompt: None,
        monitor: None,
        uncapped,
        next_capture: None,
        quit: false,
    };

    wam.create_prompt(&qh);

    loop {
        conn.flush()?;
        if let Some(deadline) = wam.next_capture {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            wam.fire_next_capture(&qh);
        }
        event_queue.blocking_dispatch(&mut wam)?;

        if wam.quit {
            break;
        }
    }

    Ok(())
}

impl Wam {
    fn create_window(&mut self, title: &str, qh: &QueueHandle<Wam>) -> Option<Window> {
        let surface = Surface::new(&self.compositor_state, qh)
            .inspect_err(|err| eprintln!("wam: failed to create surface: {err}"))
            .ok()?;
        let window = self
            .xdg_shell
            .create_window(surface, WindowDecorations::ServerDefault, qh);
        window.set_title(title);
        window.set_app_id(APP_ID);
        self.window_output = None;
        Some(window)
    }

    fn info_dpi(&self, output: &wl_output::WlOutput) -> Option<f32> {
        let info = self.output_state.info(output)?;
        let mode = info.modes.iter().find(|m| m.current).or_else(|| info.modes.first())?;
        Some(output_dpi(info.physical_size, mode.dimensions))
    }

    fn current_dpi(&self) -> f32 {
        if let Some(output) = self.window_output.as_ref()
            && let Some(dpi) = self.info_dpi(output)
        {
            return dpi;
        }
        self.output_state
            .outputs()
            .next()
            .and_then(|output| self.info_dpi(&output))
            .unwrap_or(96.0)
    }

    fn update_font_dpi(&mut self) {
        let dpi = self.current_dpi();
        if (dpi - self.dpi).abs() < 0.01 {
            return;
        }
        self.dpi = dpi;
        self.font.set_dpi(dpi);
        if let Some(prompt) = self.prompt.as_mut() {
            let (w, h) = prompt::size(&self.font);
            prompt.surface.window.set_min_size(Some((w, h)));
            prompt.surface.window.set_max_size(Some((w, h)));
            prompt.surface.window.commit();
        }
        self.redraw_monitor();
    }

    fn app_entry(&self, handle: &ExtForeignToplevelHandleV1) -> Option<AppEntry> {
        let info = self.foreign_toplevel_list.info(handle)?;
        if info.app_id == APP_ID {
            return None;
        }
        Some(AppEntry {
            handle: handle.clone(),
            title: info.title,
            app_id: info.app_id,
        })
    }

    fn create_prompt(&mut self, qh: &QueueHandle<Wam>) {
        let Some(window) = self.create_window("wam", qh) else {
            return;
        };
        let (w, h) = prompt::size(&self.font);
        window.set_min_size(Some((w, h)));
        window.set_max_size(Some((w, h)));
        window.commit();
        let mut prompt = Prompt::new(window, &self.font);
        for handle in self.foreign_toplevel_list.toplevels() {
            if let Some(entry) = self.app_entry(handle) {
                prompt.add_app(entry);
            }
        }
        self.prompt = Some(prompt);
    }

    fn alloc_window_buffer(&mut self, w: u32, h: u32) -> Buffer {
        let format = if self.shm.formats().contains(&wl_shm::Format::Argb8888) {
            wl_shm::Format::Argb8888
        } else {
            wl_shm::Format::Xrgb8888
        };
        self.pool
            .create_buffer(w as i32, h as i32, (w * 4) as i32, format)
            .expect("failed to allocate window buffer")
            .0
    }

    fn redraw_prompt(&mut self) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.redraw(&mut self.pool, &self.font);
        }
    }

    fn redraw_monitor(&mut self) {
        if let Some(monitor) = self.monitor.as_mut() {
            monitor.redraw(&mut self.pool, &self.font);
        }
    }

    fn handle_key(&mut self, keysym: Keysym, qh: &QueueHandle<Wam>) {
        match keysym {
            Keysym::Up | Keysym::Down => {
                let delta = if keysym == Keysym::Up { -1 } else { 1 };
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.move_selection(delta);
                    self.redraw_prompt();
                }
            }
            Keysym::Return | Keysym::KP_Enter => {
                if let Some(prompt) = self.prompt.as_ref()
                    && let Some(app) = prompt.apps.get(prompt.selected).cloned()
                {
                    self.start_monitoring(app, qh);
                }
            }
            Keysym::Escape => {
                if self.monitor.is_some() {
                    self.stop_monitoring(qh);
                } else {
                    self.quit = true;
                }
            }
            _ => {}
        }
    }

    fn capture_interval(&self) -> Option<Duration> {
        if self.uncapped {
            return None;
        }
        let Some(output) = (match self.window_output.as_ref() {
            Some(output) => Some(output.clone()),
            None => self.output_state.outputs().next(),
        }) else {
            return None;
        };
        let info = self.output_state.info(&output)?;
        let mode = info
            .modes
            .iter()
            .find(|m| m.current)
            .or_else(|| info.modes.first())?;
        if mode.refresh_rate > 0 {
            Some(Duration::from_secs_f64(2000.0 / mode.refresh_rate as f64))
        } else {
            None
        }
    }

    fn request_capture_paced(&mut self, qh: &QueueHandle<Wam>) {
        let deadline = self.capture_interval().map(|iv| Instant::now() + iv);
        match deadline {
            Some(deadline) => self.next_capture = Some(deadline),
            None => {
                if let Some(monitor) = self.monitor.as_mut() {
                    monitor.request_capture(qh);
                }
            }
        }
    }

    fn fire_next_capture(&mut self, qh: &QueueHandle<Wam>) {
        self.next_capture = None;
        if let Some(monitor) = self.monitor.as_mut() {
            monitor.request_capture(qh);
        }
    }

    fn start_monitoring(&mut self, app: AppEntry, qh: &QueueHandle<Wam>) {
        let (source_manager, capture_manager) =
            match (self.capture_source_manager.as_ref(), self.capture_manager.as_ref()) {
                (Some(source_manager), Some(capture_manager)) => {
                    (source_manager.clone(), capture_manager.clone())
                }
                _ => {
                    eprintln!("wam: capture protocol not available");
                    return;
                }
            };

        self.prompt = None;
        self.next_capture = None;

        let source = source_manager.create_source(&app.handle, qh, ());
        let session = capture_manager.create_session(&source, Options::empty(), qh, ());

        let Some(window) = self.create_window(&format!("wam - {}", app.title), qh) else {
            return;
        };
        window.set_min_size(Some((monitor::DEFAULT_W, monitor::DEFAULT_H)));
        window.set_max_size(Some((monitor::DEFAULT_W, monitor::DEFAULT_H)));
        window.commit();

        self.monitor = Some(Monitor::new(window, app.handle, session));
    }

    fn stop_monitoring(&mut self, qh: &QueueHandle<Wam>) {
        self.monitor = None;
        self.next_capture = None;
        self.create_prompt(qh);
    }

    fn alloc_capture_buffer(&mut self, width: u32, height: u32, format: wl_shm::Format) {
        match self
            .pool
            .create_buffer(width as i32, height as i32, (width * 4) as i32, format)
        {
            Ok((buffer, _)) => self
                .monitor
                .as_mut()
                .unwrap()
                .set_capture_buffer(buffer, (width, height), format),
            Err(err) => eprintln!("wam: failed to allocate capture buffer: {err}"),
        }
    }

    fn on_capture_session_event(
        &mut self,
        session: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        qh: &QueueHandle<Wam>,
    ) {
        if self.monitor.as_ref().is_none_or(|m| !m.has_session(session)) {
            return;
        }

        let action = self.monitor.as_mut().unwrap().on_session_event(event);
        match action {
            SessionAction::None => {}
            SessionAction::Allocate { width, height, format } => {
                self.alloc_capture_buffer(width, height, format);
                self.redraw_monitor();
                self.monitor.as_mut().unwrap().request_capture(qh);
            }
            SessionAction::Stopped => {
                self.stop_monitoring(qh);
            }
        }
    }

    fn on_capture_frame_event(
        &mut self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        qh: &QueueHandle<Wam>,
    ) {
        if self.monitor.as_ref().is_none_or(|m| !m.has_frame(frame)) {
            return;
        }

        let action = self.monitor.as_mut().unwrap().on_frame_event(event);
        match action {
            FrameAction::None => {}
            FrameAction::Ready => {
                self
                    .monitor
                    .as_mut()
                    .unwrap()
                    .present_capture(&mut self.pool);
                self.request_capture_paced(qh);
            }
            FrameAction::Retry => self.request_capture_paced(qh),
            FrameAction::Reallocate => {
                if let Some((width, height, format)) =
                    self.monitor.as_mut().unwrap().take_pending_constraints()
                {
                    self.alloc_capture_buffer(width, height, format);
                    self.request_capture_paced(qh);
                }
            }
            FrameAction::Stopped => self.stop_monitoring(qh),
        }
    }
}

impl ProvidesRegistryState for Wam {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers! {}
}

impl CompositorHandler for Wam {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        self.window_output = Some(output.clone());
        self.update_font_dpi();
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if self.window_output.as_ref() == Some(output) {
            self.window_output = None;
        }
        self.update_font_dpi();
    }
}

impl ShmHandler for Wam {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl OutputHandler for Wam {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.update_font_dpi();
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.update_font_dpi();
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.window_output.as_ref() == Some(&output) {
            self.window_output = None;
        }
        self.update_font_dpi();
    }
}

impl SeatHandler for Wam {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Err(err) = self.seat_state.get_keyboard(qh, &seat, None)
        {
            eprintln!("wam: failed to get keyboard: {err}");
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Wam {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(event.keysym, qh);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(event.keysym, qh);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: smithay_client_toolkit::seat::keyboard::Modifiers,
        _raw_modifiers: smithay_client_toolkit::seat::keyboard::RawModifiers,
        _layout: u32,
    ) {
    }
}

impl WindowHandler for Wam {
    fn request_close(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, window: &Window) {
        if self.monitor.as_ref().is_some_and(|m| &m.surface.window == window) {
            self.stop_monitoring(qh);
        } else if self.prompt.as_ref().is_some_and(|p| &p.surface.window == window) {
            self.quit = true;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let (w, h) = match configure.new_size {
            (Some(w), Some(h)) => (w.get(), h.get()),
            _ => {
                if self.prompt.as_ref().is_some_and(|p| &p.surface.window == window) {
                    prompt::size(&self.font)
                } else {
                    self.monitor
                        .as_ref()
                        .map_or((monitor::DEFAULT_W, monitor::DEFAULT_H), |m| {
                            (m.surface.width, m.surface.height)
                        })
                }
            }
        };
        if self.prompt.as_ref().is_some_and(|p| &p.surface.window == window) {
            self.prompt.as_mut().unwrap().surface.has_configure = true;
            let buffer = self.alloc_window_buffer(w, h);
            self.prompt.as_mut().unwrap().surface.set_buffer(buffer, w, h);
            self.redraw_prompt();
        } else if self.monitor.as_ref().is_some_and(|m| &m.surface.window == window) {
            let status = self
                .monitor
                .as_ref()
                .is_some_and(|m| m.capture_size.0 == 0);
            let buffer = if status { Some(self.alloc_window_buffer(w, h)) } else { None };
            let monitor = self.monitor.as_mut().unwrap();
            monitor.surface.has_configure = true;
            match buffer {
                Some(buffer) => monitor.surface.set_buffer(buffer, w, h),
                None => {
                    monitor.surface.set_display_size(w, h);
                    monitor.present_capture(&mut self.pool);
                }
            }
            if status {
                self.redraw_monitor();
            }
        }
    }
}

impl ForeignToplevelListHandler for Wam {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelList {
        &mut self.foreign_toplevel_list
    }

    fn new_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        handle: ExtForeignToplevelHandleV1,
    ) {
        if let Some(entry) = self.app_entry(&handle)
            && let Some(prompt) = self.prompt.as_mut()
        {
            prompt.add_app(entry);
            self.redraw_prompt();
        }
    }

    fn update_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        handle: ExtForeignToplevelHandleV1,
    ) {
        let Some(info) = self.foreign_toplevel_list.info(&handle) else {
            return;
        };
        if info.app_id == APP_ID {
            return;
        }
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.update_app(&handle, info.title.clone(), info.app_id.clone());
            self.redraw_prompt();
        }
        if let Some(monitor) = self.monitor.as_ref() && monitor.monitored_handle == handle {
            monitor.surface.window.set_title(format!("wam - {}", info.title));
        }
    }

    fn toplevel_closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        handle: ExtForeignToplevelHandleV1,
    ) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.remove_app(&handle);
            self.redraw_prompt();
        }
    }
}

impl Dispatch2<ExtImageCopyCaptureSessionV1, Wam> for () {
    fn event(
        &self,
        state: &mut Wam,
        proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _conn: &Connection,
        qh: &QueueHandle<Wam>,
    ) {
        state.on_capture_session_event(proxy, event, qh);
    }
}

impl Dispatch2<ExtImageCopyCaptureFrameV1, Wam> for () {
    fn event(
        &self,
        state: &mut Wam,
        proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _conn: &Connection,
        qh: &QueueHandle<Wam>,
    ) {
        state.on_capture_frame_event(proxy, event, qh);
    }
}

impl Dispatch2<ExtImageCopyCaptureManagerV1, Wam> for () {
    fn event(
        &self,
        _state: &mut Wam,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: ext_image_copy_capture_manager_v1::Event,
        _conn: &Connection,
        _qh: &QueueHandle<Wam>,
    ) {
    }
}

impl Dispatch2<ExtImageCaptureSourceV1, Wam> for () {
    fn event(
        &self,
        _state: &mut Wam,
        _proxy: &ExtImageCaptureSourceV1,
        _event: ext_image_capture_source_v1::Event,
        _conn: &Connection,
        _qh: &QueueHandle<Wam>,
    ) {
    }
}

impl Dispatch2<ExtForeignToplevelImageCaptureSourceManagerV1, Wam> for () {
    fn event(
        &self,
        _state: &mut Wam,
        _proxy: &ExtForeignToplevelImageCaptureSourceManagerV1,
        _event: ext_foreign_toplevel_image_capture_source_manager_v1::Event,
        _conn: &Connection,
        _qh: &QueueHandle<Wam>,
    ) {
    }
}

smithay_client_toolkit::delegate_registry!(Wam);
smithay_client_toolkit::delegate_dispatch2!(Wam);

#[cfg(test)]
mod tests {
    use super::output_dpi;

    #[test]
    fn physical_dpi() {
        assert!((output_dpi((527, 296), (2560, 1440)) - 123.0).abs() < 3.0);
        assert_eq!(output_dpi((0, 0), (2560, 1440)), 96.0);
        assert_eq!(output_dpi((10, 10), (2560, 1440)), 96.0);
    }
}
