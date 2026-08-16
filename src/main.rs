mod config;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
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
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
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
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

// Nimbus Sans Narrow Bold (AGPL-3.0 with the standard PS/PDF font
// embedding exception — see assets/NimbusSansNarrow-LICENSE.txt), baked
// directly into the binary at compile time via include_bytes!. No
// runtime filesystem dependency on a specific font package being
// installed — the executable is fully self-contained.
//
// Confirmed by directly measuring glyph geometry (advance width / units
// per em) against a real flip-clock display's actual font file: its '0'
// glyph has an advance-width ratio of 0.455, versus this font's 0.456 —
// effectively identical. The regular (non-Narrow) weight measures 0.556,
// visibly wider. Font family/style NAMES embedded in a font file aren't
// always reliable (this one's internal name table just says "Bold", no
// "Narrow" or "Condensed" — likely an incomplete rename), so the actual
// rendered proportions were checked directly rather than trusting the
// metadata string.
const FONT_DATA: &[u8] = include_bytes!("../assets/NimbusSansNarrow-Bold.otf");

// How long a single digit's flip animation takes, start to finish.
const FLIP_DURATION: Duration = Duration::from_millis(260);

// Index into the formatted "HH:MM" time string for each of our 4 digit
// slots (H, H, M, M) — index 2 (the ':') is skipped since it's drawn
// separately as two static dots, not a digit slot.
const DIGIT_POS: [usize; 4] = [0, 1, 3, 4];

fn main() {
    let config = config::load();

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
    // Exclusive (not OnDemand): grab keyboard focus the moment we're shown,
    // rather than waiting for the user to click into us first. A
    // screensaver-style overlay should react to the very first keypress.
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.commit();

    let pool = SlotPool::new(1, &shm).expect("failed to create shm pool");

    let font = fontdue::Font::from_bytes(FONT_DATA, fontdue::FontSettings::default())
        .expect("failed to parse bundled font");

    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("failed to create event loop");
    let loop_handle = event_loop.handle();

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .expect("failed to insert wayland source");

    let time_format = config.hour_format.strftime();
    let now_chars: Vec<char> = chrono::Local::now().format(time_format).to_string().chars().collect();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        layer,
        qh: qh.clone(),
        font,
        time_format,
        digit_color: config.digit_color,
        background_color: config.background_color,
        card_color: config.card_color,
        width: 0,
        height: 0,
        first_configure: true,
        exit: false,
        animating: false,
        keyboard: None,
        pointer: None,
        glyphs: HashMap::new(),
        slot_x: [0.0; 4],
        baseline: 0.0,
        hinge_y: 0,
        card_rects: [(0, 0, 0, 0); 2],
        card_radius: 0,
        slots: std::array::from_fn(|i| DigitSlot { current: now_chars[DIGIT_POS[i]], anim: None }),
    };

    // The timer's only job is a coarse poll for "did the clock value
    // change" while idle. Once a flip starts, redraw pacing hands off
    // entirely to frame callbacks (see CompositorHandler::frame below) —
    // the correct Wayland way to animate, since it only draws exactly as
    // often as the compositor is ready to show a new frame.
    loop_handle
        .insert_source(Timer::immediate(), |_deadline, _, app| {
            // Guard against firing before the first `configure` event has
            // arrived (width/height still 0, glyph cache still empty).
            if app.width > 0 && app.height > 0 && !app.animating {
                app.advance();
                app.draw();
            }
            TimeoutAction::ToDuration(Duration::from_millis(300))
        })
        .expect("failed to insert timer");

    loop {
        event_loop.dispatch(Duration::from_millis(16), &mut app).unwrap();
        if app.exit {
            break;
        }
    }
}

/// One of the 4 digit positions on screen (H, H, M, M).
struct DigitSlot {
    /// The value currently considered "settled" (fully shown, no longer
    /// animating either way).
    current: char,
    /// Set while a flip from `current` to the new value is in progress.
    anim: Option<(char, Instant)>, // (new value, animation start time)
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    qh: QueueHandle<App>,
    font: fontdue::Font,
    time_format: &'static str,
    digit_color: (u8, u8, u8),
    background_color: (u8, u8, u8),
    card_color: (u8, u8, u8),
    width: u32,
    height: u32,
    first_configure: bool,
    exit: bool,
    // True while any digit slot is mid-flip. While true, redraws are paced
    // by frame callbacks instead of the idle-poll timer.
    animating: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    // Cache of rasterized glyphs for '0'..'9', keyed by character,
    // computed once per screen size instead of every frame.
    glyphs: HashMap<char, (fontdue::Metrics, Vec<u8>)>,
    // Precomputed, fixed x position for each of the 4 digit slots, so
    // digits don't jitter horizontally as they change width.
    slot_x: [f32; 4],
    baseline: f32,
    hinge_y: i32,
    // (x0, y0, x1, y1) for the hour-pair card and the minute-pair card.
    card_rects: [(i32, i32, i32, i32); 2],
    card_radius: i32,

    slots: [DigitSlot; 4],
}

impl App {
    /// Rasterizes and caches every glyph we'll ever need, and computes the
    /// fixed on-screen card/slot layout. Called whenever the surface size
    /// is (re)established.
    ///
    /// Each card is a fixed SQUARE sized off screen height alone, not a
    /// width-driven rectangle sized to fit its digits (an earlier version
    /// did that, and the proportions kept drifting no matter how much the
    /// padding ratios were tuned).
    fn layout(&mut self) {
        let h = self.height as f32;
        let w = self.width as f32;

        let rectsize = h * 0.6;
        self.card_radius = (h * 0.05714).round() as i32;
        let pair_gap = w * 0.031;

        // Font size: pick the largest size where 2 digits + a small gap
        // still fit inside the fixed-size square card, same two-pass
        // shrink-if-needed idea as before, just bounded by the card's
        // own width instead of the whole screen's.
        let mut size = rectsize;
        for _ in 0..2 {
            self.glyphs.clear();
            for c in "0123456789".chars() {
                self.glyphs.insert(c, self.font.rasterize(c, size));
            }
            let digit_width = self.glyphs[&'0'].0.advance_width;
            let inner_width = 2.0 * digit_width + size * 0.02;
            let max_inner_width = rectsize * 0.88;
            if inner_width > max_inner_width {
                size *= max_inner_width / inner_width;
            } else {
                break;
            }
        }

        let digit = &self.glyphs[&'0'].0;
        let digit_width = digit.advance_width;
        let glyph_height = digit.height as f32;
        let digit_gap = size * 0.02;

        let total_width = 2.0 * rectsize + pair_gap;
        let start_x = (w - total_width) / 2.0;
        let start_y = (h - rectsize) / 2.0; // 20% top/bottom margin when rectsize = 0.6h

        let hour_x0 = start_x;
        let hour_x1 = start_x + rectsize;
        let minute_x0 = hour_x1 + pair_gap;
        let minute_x1 = minute_x0 + rectsize;

        // Center each digit pair on its card's horizontal midpoint.
        let hour_center = hour_x0 + rectsize / 2.0;
        let minute_center = minute_x0 + rectsize / 2.0;
        self.slot_x[0] = hour_center - digit_width - digit_gap / 2.0;
        self.slot_x[1] = hour_center + digit_gap / 2.0;
        self.slot_x[2] = minute_center - digit_width - digit_gap / 2.0;
        self.slot_x[3] = minute_center + digit_gap / 2.0;

        self.hinge_y = self.height as i32 / 2;
        // Center the glyph's actual vertical midpoint on the hinge, using
        // real rasterized metrics instead of a guessed offset — a glyph
        // drawn at `baseline` spans canvas rows
        // [baseline - ymin - height, baseline - ymin), so its midpoint is
        // baseline - ymin - height/2; setting that equal to hinge_y and
        // solving for baseline gives this.
        self.baseline = self.hinge_y as f32 + digit.ymin as f32 + glyph_height / 2.0;

        self.card_rects[0] = (hour_x0 as i32, start_y as i32, hour_x1 as i32, (start_y + rectsize) as i32);
        self.card_rects[1] = (minute_x0 as i32, start_y as i32, minute_x1 as i32, (start_y + rectsize) as i32);
    }

    /// Checks the real clock and starts/advances/settles any flip
    /// animations. Pure state update — does not draw. Updates
    /// `self.animating` to reflect whether anything is still mid-flip.
    fn advance(&mut self) {
        if self.width == 0 || self.height == 0 {
            return;
        }

        let now_str = chrono::Local::now().format(self.time_format).to_string();
        let now_chars: Vec<char> = now_str.chars().collect();

        let mut animating = false;
        for i in 0..4 {
            let target = now_chars[DIGIT_POS[i]];
            if let Some((new_ch, start)) = self.slots[i].anim {
                let progress = start.elapsed().as_secs_f32() / FLIP_DURATION.as_secs_f32();
                if progress >= 1.0 {
                    self.slots[i].current = new_ch;
                    self.slots[i].anim = None;
                } else {
                    animating = true;
                }
            }
            if self.slots[i].anim.is_none() && self.slots[i].current != target {
                self.slots[i].anim = Some((target, Instant::now()));
                animating = true;
            }
        }

        self.animating = animating;
    }

    fn draw(&mut self) {
        let (width, height) = (self.width, self.height);
        let stride = width as i32 * 4;

        let (buffer, canvas) = self
            .pool
            .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
            .expect("create buffer");

        let bg = self.background_color;
        canvas.chunks_exact_mut(4).for_each(|pixel| {
            pixel.copy_from_slice(&[bg.2, bg.1, bg.0, 0xFF]);
        });

        // The two card panels: a single flat fill, nothing more. Extra
        // chrome (grain, borders, highlight lines, drop shadows) was tried
        // and reads as clutter rather than "premium" — real flip-clock
        // displays are this minimal on purpose.
        for &(x0, y0, x1, y1) in &self.card_rects {
            fill_rounded_rect(canvas, width, height, x0, y0, x1, y1, self.card_radius, self.card_color);
        }

        // The hinge: a thin black gap plus a light 1px line, not a shadow
        // gradient. Drawn BEFORE the digits so it sits under them.
        for &(x0, _, x1, _) in &self.card_rects {
            draw_hinge_line(canvas, width, height, x0, x1, self.hinge_y, height);
        }

        let fg = self.digit_color;
        for i in 0..4 {
            let pen_x = self.slot_x[i];
            let slot = &self.slots[i];

            match slot.anim {
                None => {
                    let (m, b) = &self.glyphs[&slot.current];
                    draw_glyph_half(canvas, width, height, pen_x, self.baseline, m, b, self.hinge_y, Half::Top, 1.0, fg);
                    draw_glyph_half(canvas, width, height, pen_x, self.baseline, m, b, self.hinge_y, Half::Bottom, 1.0, fg);
                }
                Some((new_ch, start)) => {
                    let progress = (start.elapsed().as_secs_f32() / FLIP_DURATION.as_secs_f32()).min(1.0);
                    let old_ch = slot.current;
                    let (old_m, old_b) = &self.glyphs[&old_ch];
                    let (new_m, new_b) = &self.glyphs[&new_ch];

                    // Ease across the FULL 0..1 progress once, then split
                    // the already-eased value into the two phases — not
                    // two separate smoothstep(0..1) calls. smoothstep's
                    // derivative is zero at both ends of whatever range you
                    // give it, so easing each phase independently makes
                    // the motion stop dead at the 50% mark (end of phase 1,
                    // start of phase 2) and re-accelerate from rest right
                    // after — a visible stutter at the hinge crossover on
                    // every flip. Easing the whole span instead means
                    // velocity is zero only at the true start/end of the
                    // flip and fastest right through the middle, which is
                    // one continuous motion with no pause.
                    let eased = smoothstep(progress);

                    if eased < 0.5 {
                        let q = eased / 0.5;
                        // New top revealed underneath as the old top shrinks away.
                        draw_glyph_half(canvas, width, height, pen_x, self.baseline, new_m, new_b, self.hinge_y, Half::Top, 1.0, fg);
                        draw_glyph_half(canvas, width, height, pen_x, self.baseline, old_m, old_b, self.hinge_y, Half::Top, 1.0 - q, fg);
                        // Bottom hasn't started changing yet.
                        draw_glyph_half(canvas, width, height, pen_x, self.baseline, old_m, old_b, self.hinge_y, Half::Bottom, 1.0, fg);
                    } else {
                        let q = (eased - 0.5) / 0.5;
                        // Top settled onto its new value already.
                        draw_glyph_half(canvas, width, height, pen_x, self.baseline, new_m, new_b, self.hinge_y, Half::Top, 1.0, fg);
                        // New bottom grows in from the hinge, covering the old one.
                        draw_glyph_half(canvas, width, height, pen_x, self.baseline, old_m, old_b, self.hinge_y, Half::Bottom, 1.0, fg);
                        draw_glyph_half(canvas, width, height, pen_x, self.baseline, new_m, new_b, self.hinge_y, Half::Bottom, q, fg);
                    }
                }
            }
        }

        self.layer.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(self.layer.wl_surface()).expect("attach buffer");

        // If still animating, ask for another frame callback BEFORE
        // committing, so the request rides along in this same commit
        // instead of sitting unsent until some later commit (wl_surface's
        // pending `frame` request only actually reaches the compositor on
        // the next commit — requesting it after committing is a no-op
        // until something else commits again, which never happens if
        // nothing else is driving redraws).
        if self.animating {
            let surface = self.layer.wl_surface();
            surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        }

        self.layer.commit();
    }
}

/// Smoothstep easing: eases in and out, used to make the flip decelerate
/// into place instead of moving at a constant linear rate.
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[derive(Clone, Copy, PartialEq)]
enum Half {
    Top,
    Bottom,
}

/// Draws one half (top or bottom, split at `hinge_y`) of a rasterized
/// glyph, squished vertically by `scale` (1.0 = full size, 0.0 = collapsed
/// to nothing) and anchored at the hinge line. This is the 2D stand-in for
/// a card rotating around its hinge in 3D: shrinking a flat half toward its
/// fixed edge reads as it rotating away edge-on.
#[allow(clippy::too_many_arguments)]
fn draw_glyph_half(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    pen_x: f32,
    baseline: f32,
    metrics: &fontdue::Metrics,
    bitmap: &[u8],
    hinge_y: i32,
    half: Half,
    scale: f32,
    fg: (u8, u8, u8),
) {
    if scale <= 0.0 || metrics.width == 0 || metrics.height == 0 {
        return;
    }

    // A card rotating toward edge-on catches light at an ever-grazing
    // angle, so it visibly darkens as it approaches the hinge — this is
    // near-zero at scale=1 (flat on, full color; a static/background half
    // is unaffected) and strongest right at scale=0 (edge-on).
    let darken = (1.0 - scale).clamp(0.0, 1.0) * 0.45;
    let fg = (
        (fg.0 as f32 * (1.0 - darken)) as u8,
        (fg.1 as f32 * (1.0 - darken)) as u8,
        (fg.2 as f32 * (1.0 - darken)) as u8,
    );

    // Canvas y where bitmap row 0 would land, if drawn unscaled.
    let glyph_top = baseline as i32 - metrics.ymin - metrics.height as i32;
    let hinge_row = (hinge_y - glyph_top) as f32;

    let (src_start, src_end) = match half {
        Half::Top => (0.0, hinge_row.clamp(0.0, metrics.height as f32)),
        Half::Bottom => (hinge_row.clamp(0.0, metrics.height as f32), metrics.height as f32),
    };
    let src_span = (src_end - src_start).max(0.0);
    if src_span <= 0.0 {
        return;
    }
    let out_span = (src_span * scale).ceil() as i32;

    for out_row in 0..out_span {
        let (canvas_y, src_row_f) = match half {
            // Anchored at its bottom edge (the hinge); grows upward.
            Half::Top => (hinge_y - 1 - out_row, src_end - 1.0 - out_row as f32 / scale),
            // Anchored at its top edge (the hinge); grows downward.
            Half::Bottom => (hinge_y + out_row, src_start + out_row as f32 / scale),
        };
        if canvas_y < 0 || canvas_y as u32 >= height {
            continue;
        }
        let src_row = src_row_f.round() as i32;
        if src_row < 0 || src_row as u32 >= metrics.height as u32 {
            continue;
        }

        for x in 0..metrics.width {
            let coverage = bitmap[src_row as usize * metrics.width + x];
            if coverage == 0 {
                continue;
            }
            let px = pen_x as i32 + metrics.xmin + x as i32;
            if px < 0 || px as u32 >= width {
                continue;
            }
            let idx = (canvas_y as u32 * width + px as u32) as usize * 4;
            // Blend from whatever's currently there (the card background,
            // or an earlier-drawn layer) toward the digit color, weighted
            // by this pixel's coverage — gives free antialiasing in any
            // configured color pair, not just white-on-black.
            let t = coverage as f32 / 255.0;
            canvas[idx] = lerp_u8(canvas[idx], fg.2, t);
            canvas[idx + 1] = lerp_u8(canvas[idx + 1], fg.1, t);
            canvas[idx + 2] = lerp_u8(canvas[idx + 2], fg.0, t);
            canvas[idx + 3] = 0xFF;
        }
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Fills an axis-aligned rectangle with rounded corners. Standard
/// "clamp to nearest corner center, test distance" technique: pixels in
/// the straight top/bottom/left/right bands always pass (clamping puts the
/// test center right on the pixel itself); only pixels in the four corner
/// squares actually get a circular cutoff.
#[allow(clippy::too_many_arguments)]
fn fill_rounded_rect(canvas: &mut [u8], width: u32, height: u32, x0: i32, y0: i32, x1: i32, y1: i32, radius: i32, color: (u8, u8, u8)) {
    let radius = radius.max(0);
    for y in y0.max(0)..y1.min(height as i32) {
        for x in x0.max(0)..x1.min(width as i32) {
            let cx = x.clamp(x0 + radius, (x1 - radius).max(x0 + radius));
            let cy = y.clamp(y0 + radius, (y1 - radius).max(y0 + radius));
            let dx = x - cx;
            let dy = y - cy;
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            let idx = (y as u32 * width + x as u32) as usize * 4;
            canvas[idx] = color.2;
            canvas[idx + 1] = color.1;
            canvas[idx + 2] = color.0;
            canvas[idx + 3] = 0xFF;
        }
    }
}

/// Draws the hinge as a thin BLACK gap band (0.5% of screen height) at the
/// split, followed immediately by a single 1px line in a fixed light grey
/// (0x1a) — not a shadow gradient. The band is what actually reads as
/// "two separate cards meeting," and the line beneath it
/// is a subtle highlight, not a shadow (it's lighter than the card, not
/// darker).
fn draw_hinge_line(canvas: &mut [u8], width: u32, height: u32, x0: i32, x1: i32, hinge_y: i32, screen_height: u32) {
    let band_h = ((screen_height as f32 * 0.005).round() as i32).max(1);
    let band_y0 = hinge_y - band_h / 2;

    for dy in 0..band_h {
        let y = band_y0 + dy;
        if y < 0 || y as u32 >= height {
            continue;
        }
        let idx_row = y as u32 * width;
        for x in x0.max(0)..x1.min(width as i32) {
            let idx = (idx_row + x as u32) as usize * 4;
            canvas[idx] = 0;
            canvas[idx + 1] = 0;
            canvas[idx + 2] = 0;
        }
    }

    let line_y = band_y0 + band_h;
    if line_y >= 0 && (line_y as u32) < height {
        let idx_row = line_y as u32 * width;
        for x in x0.max(0)..x1.min(width as i32) {
            let idx = (idx_row + x as u32) as usize * 4;
            canvas[idx] = 0x1a;
            canvas[idx + 1] = 0x1a;
            canvas[idx + 2] = 0x1a;
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // The compositor is ready for a new frame. If we're mid-flip,
        // advance the animation and redraw — draw() itself will ask for
        // another frame callback if still animating afterward, keeping
        // this loop going at the compositor's own pace.
        if self.animating {
            self.advance();
            self.draw();
        }
    }
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
        self.layout();
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

// Dismiss-on-any-input: grab keyboard and pointer devices as they show
// up, and exit on the first real interaction.
impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            self.keyboard.take();
        }
        if capability == Capability::Pointer {
            self.pointer.take();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {
        self.exit = true;
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: RawModifiers, _: u32) {}
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            // Ignore Enter/Leave — those fire just from the surface
            // appearing under a stationary cursor, which would otherwise
            // instantly dismiss us on launch. Only real interaction
            // (movement, clicks, scroll) counts.
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Leave { .. } => {}
                _ => self.exit = true,
            }
        }
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
