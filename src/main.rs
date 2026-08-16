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

// Oswald Bold (OFL-1.1 licensed, see assets/Oswald-LICENSE.txt), baked
// directly into the binary at compile time via include_bytes!. No runtime
// filesystem dependency on a specific font package being installed — the
// executable is fully self-contained. Oswald is a genuinely condensed
// typeface by design (unlike our first attempt, Inter ExtraBold, which we
// tried to force-narrow with a horizontal scale hack — that just made the
// strokes look unevenly stretched, since squeezing only one axis distorts
// a typeface that wasn't designed for it). A real condensed face keeps its
// stroke proportions consistent while still being narrow enough that 4
// digits comfortably fit a 16:9 screen's width with plenty of height to
// spare — which is what actually lets the clock get big like real Fliqlo.
const FONT_DATA: &[u8] = include_bytes!("../assets/Oswald-Bold.ttf");

// How long a single digit's flip animation takes, start to finish.
const FLIP_DURATION: Duration = Duration::from_millis(350);

// Even Oswald's condensed proportions leave the clock a bit short of a
// screen's full height on standard 16:9 monitors (we're still width-bound
// before height runs out). A MILD additional horizontal squeeze closes
// that gap — unlike the earlier 0.62 squeeze on Inter ExtraBold (which
// was forcing a wide, non-condensed face to be dramatically narrower and
// visibly distorted its strokes), this is a small nudge on top of a face
// that's already properly condensed, so it stays clean at this ratio.
const DIGIT_SQUEEZE: f32 = 0.85;

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
        hinge_band: 0,
        shadow_offset: 0,
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
    // How many pixels above/below the hinge the fold shadow fades out over.
    hinge_band: i32,
    // Offset (both x and y) for the settled-digit drop shadow.
    shadow_offset: i32,

    slots: [DigitSlot; 4],
}

impl App {
    /// Rasterizes and caches every glyph we'll ever need, and computes the
    /// fixed on-screen card/slot layout. Called whenever the surface size
    /// is (re)established.
    fn layout(&mut self) {
        // Start from a generous height-based size, then shrink it if that
        // would overflow the available width — this way it always uses the
        // largest size that actually fits THIS monitor's aspect ratio,
        // rather than a single fixed ratio that's oversized on ultrawide
        // screens and overflows on standard 16:9/16:10 ones.
        let mut size = self.height as f32 * 1.3;
        for _ in 0..2 {
            self.glyphs.clear();
            for c in "0123456789".chars() {
                self.glyphs.insert(c, self.font.rasterize(c, size));
            }
            let digit_width = self.glyphs[&'0'].0.advance_width * DIGIT_SQUEEZE;
            let card_width = 2.0 * digit_width + size * 0.015 + 2.0 * (size * 0.03);
            let total_width = 2.0 * card_width + size * 0.18;

            let max_width = self.width as f32 * 0.99;
            if total_width > max_width {
                size *= max_width / total_width;
            } else {
                break;
            }
        }

        let digit = &self.glyphs[&'0'].0;
        let digit_width = digit.advance_width * DIGIT_SQUEEZE;
        let glyph_height = digit.height as f32;

        let card_padding_x = size * 0.03;
        let card_padding_y = size * 0.05;
        let digit_gap = size * 0.015;
        let pair_gap = size * 0.18;

        let card_width = 2.0 * digit_width + digit_gap + 2.0 * card_padding_x;
        let half_card_height = (glyph_height / 2.0 + card_padding_y).round() as i32;

        let total_width = 2.0 * card_width + pair_gap;
        let start_x = (self.width as f32 - total_width) / 2.0;

        let hour_x0 = start_x;
        let hour_x1 = start_x + card_width;
        let minute_x0 = hour_x1 + pair_gap;
        let minute_x1 = minute_x0 + card_width;

        self.slot_x[0] = hour_x0 + card_padding_x;
        self.slot_x[1] = self.slot_x[0] + digit_width + digit_gap;
        self.slot_x[2] = minute_x0 + card_padding_x;
        self.slot_x[3] = self.slot_x[2] + digit_width + digit_gap;

        self.hinge_y = self.height as i32 / 2;
        // Center the glyph's actual vertical midpoint on the hinge, using
        // real rasterized metrics instead of a guessed offset (size/3 was
        // tuned by eye for the old font and left Oswald visibly off-center
        // within its card — different fonts have different ymin/cap-height
        // proportions, so this has to be derived per-font, not hardcoded).
        // A glyph drawn at `baseline` spans canvas rows
        // [baseline - ymin - height, baseline - ymin), so its midpoint is
        // baseline - ymin - height/2; setting that equal to hinge_y and
        // solving for baseline gives this.
        self.baseline = self.hinge_y as f32 + digit.ymin as f32 + glyph_height / 2.0;

        let card_top = self.hinge_y - half_card_height;
        let card_bottom = self.hinge_y + half_card_height;
        self.card_rects[0] = (hour_x0 as i32, card_top, hour_x1 as i32, card_bottom);
        self.card_rects[1] = (minute_x0 as i32, card_top, minute_x1 as i32, card_bottom);
        self.card_radius = (size * 0.08).round() as i32;
        self.hinge_band = ((size * 0.06).round() as i32).max(2);

        self.shadow_offset = (size * 0.012).round().max(1.0) as i32;
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

        // The two card panels sit behind everything else — digits and the
        // hinge shadow both draw on top of them. A hairline border (a
        // slightly lighter ring, drawn first and then inset by the fill)
        // gives each card a precise, "engineered object" edge instead of
        // just fading straight into the black background.
        let border_color = (
            (self.card_color.0 as u16 + 14).min(255) as u8,
            (self.card_color.1 as u16 + 14).min(255) as u8,
            (self.card_color.2 as u16 + 14).min(255) as u8,
        );
        for &(x0, y0, x1, y1) in &self.card_rects {
            fill_rounded_rect(canvas, width, height, x0, y0, x1, y1, self.card_radius, border_color);
        }
        for &(x0, y0, x1, y1) in &self.card_rects {
            fill_rounded_rect(canvas, width, height, x0 + 1, y0 + 1, x1 - 1, y1 - 1, (self.card_radius - 1).max(0), self.card_color);
        }

        // A subtle film-grain texture on each card — a flat vector fill
        // reads as a UI panel; a very slight per-pixel brightness jitter
        // reads as an actual physical material sitting under the light.
        for &(x0, y0, x1, y1) in &self.card_rects {
            apply_grain(canvas, width, height, x0, y0, x1, y1, 5.0);
        }

        // A faint top-edge highlight on each card — a hint of light
        // catching a beveled edge, which reads as physical depth even
        // though it's just one brighter line.
        let highlight = (
            (self.card_color.0 as u16 + 22).min(255) as u8,
            (self.card_color.1 as u16 + 22).min(255) as u8,
            (self.card_color.2 as u16 + 22).min(255) as u8,
        );
        for &(x0, y0, x1, _) in &self.card_rects {
            for x in (x0 + self.card_radius).max(0)..(x1 - self.card_radius).min(width as i32) {
                let y = y0 + 1;
                if y < 0 || y as u32 >= height {
                    continue;
                }
                let idx = (y as u32 * width + x as u32) as usize * 4;
                canvas[idx] = highlight.2;
                canvas[idx + 1] = highlight.1;
                canvas[idx + 2] = highlight.0;
            }
        }

        // A soft fold shadow at each card's hinge, instead of a hard line —
        // darkens a small band that fades out above and below the hinge.
        // Drawn BEFORE the digits (not after) so it only affects the card
        // surface, not the digit ink itself — darkening on top of already-
        // drawn glyphs created ugly blobby artifacts wherever the shadow
        // band crossed a thin or curved stroke (the loop of a 6, the foot
        // of a 1). The digits render cleanly on top of the (now slightly
        // pre-darkened) card instead.
        for &(x0, _, x1, _) in &self.card_rects {
            draw_hinge_shadow(canvas, width, height, x0, x1, self.hinge_y, self.hinge_band);
        }

        let fg = self.digit_color;
        for i in 0..4 {
            let pen_x = self.slot_x[i];
            let slot = &self.slots[i];

            match slot.anim {
                None => {
                    let (m, b) = &self.glyphs[&slot.current];
                    // A soft offset shadow under the settled digit — gives
                    // the typography actual lift off the card surface
                    // instead of looking pasted flat onto it. Skipped
                    // during an active flip (the fast motion there hides
                    // its absence, and it'd need to follow both halves
                    // through the animation for little visible benefit).
                    draw_glyph_shadow(canvas, width, height, pen_x, self.baseline, m, b, self.shadow_offset, 0.35);
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

/// Draws a soft, offset silhouette of a full (unsplit, unscaled) glyph at
/// reduced opacity — a simple drop shadow that gives settled typography a
/// bit of lift off the card surface instead of looking pasted flat onto
/// it. Unlike draw_glyph_half, this always draws the complete glyph (no
/// hinge split) since it's only used for non-animating digits.
#[allow(clippy::too_many_arguments)]
fn draw_glyph_shadow(canvas: &mut [u8], width: u32, height: u32, pen_x: f32, baseline: f32, metrics: &fontdue::Metrics, bitmap: &[u8], offset: i32, opacity: f32) {
    for y in 0..metrics.height {
        let canvas_y = baseline as i32 - metrics.ymin - metrics.height as i32 + y as i32 + offset;
        if canvas_y < 0 || canvas_y as u32 >= height {
            continue;
        }
        let squeezed_width = ((metrics.width as f32) * DIGIT_SQUEEZE).ceil() as i32;
        for ox in 0..squeezed_width {
            let src_x = (ox as f32 / DIGIT_SQUEEZE).round() as usize;
            if src_x >= metrics.width {
                continue;
            }
            let coverage = bitmap[y * metrics.width + src_x];
            if coverage == 0 {
                continue;
            }
            let px = pen_x as i32 + (metrics.xmin as f32 * DIGIT_SQUEEZE).round() as i32 + ox + offset;
            if px < 0 || px as u32 >= width {
                continue;
            }
            let idx = (canvas_y as u32 * width + px as u32) as usize * 4;
            let t = (coverage as f32 / 255.0) * opacity;
            canvas[idx] = lerp_u8(canvas[idx], 0, t);
            canvas[idx + 1] = lerp_u8(canvas[idx + 1], 0, t);
            canvas[idx + 2] = lerp_u8(canvas[idx + 2], 0, t);
        }
    }
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

        let squeezed_width = ((metrics.width as f32) * DIGIT_SQUEEZE).ceil() as i32;
        for ox in 0..squeezed_width {
            let src_x = (ox as f32 / DIGIT_SQUEEZE).round() as usize;
            if src_x >= metrics.width {
                continue;
            }
            let coverage = bitmap[src_row as usize * metrics.width + src_x];
            if coverage == 0 {
                continue;
            }
            let px = pen_x as i32 + (metrics.xmin as f32 * DIGIT_SQUEEZE).round() as i32 + ox;
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

/// Darkens a band of `canvas` around `hinge_y` (within x0..x1), fading out
/// toward the edges of the band — a soft fold shadow instead of a hard
/// line, drawn on top of whatever's already there (card + digits).
fn draw_hinge_shadow(canvas: &mut [u8], width: u32, height: u32, x0: i32, x1: i32, hinge_y: i32, band: i32) {
    for dy in -band..=band {
        let y = hinge_y + dy;
        if y < 0 || y as u32 >= height {
            continue;
        }
        let t = 1.0 - (dy.abs() as f32 / band as f32);
        let darken = t * 0.55;
        for x in x0.max(0)..x1.min(width as i32) {
            let idx = (y as u32 * width + x as u32) as usize * 4;
            canvas[idx] = (canvas[idx] as f32 * (1.0 - darken)) as u8;
            canvas[idx + 1] = (canvas[idx + 1] as f32 * (1.0 - darken)) as u8;
            canvas[idx + 2] = (canvas[idx + 2] as f32 * (1.0 - darken)) as u8;
        }
    }
}

/// Adds a subtle, deterministic per-pixel brightness jitter within a
/// rectangle — a flat color fill reads as a vector UI panel; a slight
/// grain reads as an actual physical material under light. Deterministic
/// (same noise value for a given x,y every frame) so it doesn't shimmer
/// like TV static — it's meant to be felt, not really "seen".
fn apply_grain(canvas: &mut [u8], width: u32, height: u32, x0: i32, y0: i32, x1: i32, y1: i32, strength: f32) {
    for y in y0.max(0)..y1.min(height as i32) {
        for x in x0.max(0)..x1.min(width as i32) {
            let n = grain_noise(x, y) * strength;
            let idx = (y as u32 * width + x as u32) as usize * 4;
            canvas[idx] = (canvas[idx] as f32 + n).clamp(0.0, 255.0) as u8;
            canvas[idx + 1] = (canvas[idx + 1] as f32 + n).clamp(0.0, 255.0) as u8;
            canvas[idx + 2] = (canvas[idx + 2] as f32 + n).clamp(0.0, 255.0) as u8;
        }
    }
}

/// A cheap deterministic pseudo-random value in [-1, 1] for integer pixel
/// coordinates — an integer hash, not an actual RNG, so no state to carry
/// around and the same (x,y) always gives the same value.
fn grain_noise(x: i32, y: i32) -> f32 {
    let n = (x.wrapping_mul(374761393).wrapping_add(y.wrapping_mul(668265263))) as u32;
    let n = (n ^ (n >> 13)).wrapping_mul(1274126177);
    let v = (n >> 8) & 0xFFFF;
    (v as f32 / 65535.0) * 2.0 - 1.0
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

// Dismiss-on-any-input, matching gluqlo's `-anykeyclose`: grab keyboard and
// pointer devices as they show up, and exit on the first real interaction.
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
