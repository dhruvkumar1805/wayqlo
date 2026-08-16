use std::time::Duration;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

// The font used to draw the clock digits. Loaded from the system for now;
// we'll bundle our own font file once we get to the flip-card look.
const FONT_PATH: &str = "/usr/share/fonts/noto/NotoSans-Bold.ttf";

fn main() {
    let conn = Connection::connect_to_env().expect("failed to connect to Wayland compositor");
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell not available on this compositor");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm not available");

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("wayqlo"), None);

    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    layer.commit();

    let pool = SlotPool::new(1, &shm).expect("failed to create shm pool");

    let font_bytes = std::fs::read(FONT_PATH).expect("failed to read font file");
    let font = fontdue::Font::from_bytes(font_bytes, fontdue::FontSettings::default())
        .expect("failed to parse font");

    // The event loop that will drive us: it can wait on the Wayland socket
    // AND a timer at the same time, waking us up whenever either has
    // something to do.
    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("failed to create event loop");
    let loop_handle = event_loop.handle();

    // Register the Wayland connection as a source the event loop watches.
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .expect("failed to insert wayland source");

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        font,
        width: 0,
        height: 0,
        first_configure: true,
        exit: false,
    };

    // A timer that fires once immediately, then reschedules itself every
    // second, redrawing the clock each time.
    loop_handle
        .insert_source(Timer::immediate(), |_deadline, _, app| {
            app.redraw_if_ready();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .expect("failed to insert timer");

    loop {
        event_loop.dispatch(Duration::from_millis(16), &mut app).unwrap();
        if app.exit {
            break;
        }
    }
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    font: fontdue::Font,
    width: u32,
    height: u32,
    first_configure: bool,
    exit: bool,
}

impl App {
    fn redraw_if_ready(&mut self) {
        if self.width > 0 && self.height > 0 {
            self.draw();
        }
    }

    fn draw(&mut self) {
        let (width, height) = (self.width, self.height);
        let stride = width as i32 * 4;

        let (buffer, canvas) = self
            .pool
            .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
            .expect("create buffer");

        // Fill every pixel with opaque black (ARGB byte order = B,G,R,A).
        canvas.chunks_exact_mut(4).for_each(|pixel| {
            pixel.copy_from_slice(&[0x00, 0x00, 0x00, 0xFF]);
        });

        let text = chrono::Local::now().format("%H:%M").to_string();
        draw_text(canvas, width, height, &self.font, &text);

        self.layer.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(self.layer.wl_surface()).expect("attach buffer");
        self.layer.commit();
    }
}

/// Rasterizes `text` with fontdue and blits it, centered, in white onto
/// `canvas` (an ARGB8888 buffer of size width*height*4 bytes).
fn draw_text(canvas: &mut [u8], width: u32, height: u32, font: &fontdue::Font, text: &str) {
    let size = height as f32 * 0.35;

    // First pass: rasterize every glyph and measure total width, so we can
    // center the whole string horizontally.
    let glyphs: Vec<(fontdue::Metrics, Vec<u8>)> =
        text.chars().map(|c| font.rasterize(c, size)).collect();

    let total_width: f32 = glyphs.iter().map(|(m, _)| m.advance_width).sum();
    let mut pen_x = (width as f32 - total_width) / 2.0;
    let baseline = height as f32 / 2.0 + size / 3.0;

    for (metrics, bitmap) in &glyphs {
        for y in 0..metrics.height {
            for x in 0..metrics.width {
                let coverage = bitmap[y * metrics.width + x];
                if coverage == 0 {
                    continue;
                }
                let px = pen_x as i32 + metrics.xmin + x as i32;
                let py = baseline as i32 - metrics.ymin - metrics.height as i32 + y as i32;
                if px < 0 || py < 0 || px as u32 >= width || py as u32 >= height {
                    continue;
                }
                let idx = (py as u32 * width + px as u32) as usize * 4;
                // White text: just set B, G, R to the glyph's coverage value.
                canvas[idx] = coverage;
                canvas[idx + 1] = coverage;
                canvas[idx + 2] = coverage;
                canvas[idx + 3] = 0xFF;
            }
        }
        pen_x += metrics.advance_width;
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        self.width = configure.new_size.0;
        self.height = configure.new_size.1;
        if self.first_configure {
            self.first_configure = false;
            self.draw();
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
