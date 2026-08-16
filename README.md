# wayqlo

A native Wayland flip-clock screensaver, built for Hyprland, Sway, and other
wlroots based compositors. No XWayland, no dependency on any X11 screensaver
framework.

## Features

- Fullscreen flip-clock display drawn directly with `wlr-layer-shell`
- Smooth, eased flip animation paced to the compositor's own frame callbacks
- Exits on any keypress or pointer movement
- Configurable colors and 12/24 hour format
- Single self-contained binary, font included at compile time

## Installing

Requires a Rust toolchain (`cargo`).

```sh
git clone https://github.com/dhruvkumar1805/wayqlo.git
cd wayqlo
./install.sh
```

This builds the release binary, installs it to `~/.local/bin/wayqlo`, and
wires it into your idle daemon automatically:

- **Omarchy:** takes over the existing screensaver hook in
  `hypridle.conf` (backs the file up first).
- **Plain Hyprland (hypridle):** appends a listener block to
  `hypridle.conf` if one is not already there (backs the file up first).
- **Sway (swayidle):** sway configs vary too much to edit safely, so it
  prints the line to add yourself. See `contrib/swayidle.sh`.

Safe to re-run, every check is idempotent. If you would rather do it by
hand, build with `cargo build --release` and see `contrib/` for the raw
config snippets.

## Running

```sh
wayqlo
```

It goes fullscreen immediately on your primary output and grabs keyboard
focus. Press any key or move the mouse to exit.

## Configuration

wayqlo reads `~/.config/wayqlo/config.toml` if it exists. Any field you leave
out falls back to its default. See `contrib/config.toml` for a full example.

| Field               | Default     | Description                    |
| ------------------- | ----------- | ------------------------------ |
| `hour_format`        | `"24"`      | `"12"` or `"24"`               |
| `digit_color`        | `"#B7B7B7"` | Digit color, as `#RRGGBB`      |
| `background_color`   | `"#000000"` | Screen background color        |
| `card_color`         | `"#0F0F0F"` | Flip card panel color          |

## License

wayqlo's source code is licensed under the MIT License, see `LICENSE`.

The bundled font (Nimbus Sans Narrow Bold) is licensed separately under
AGPL-3.0 with the standard PS/PDF font embedding exception, see
`assets/NimbusSansNarrow-LICENSE.txt`.
