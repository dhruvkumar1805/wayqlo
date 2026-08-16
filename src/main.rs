use smithay_client_toolkit::{
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use wayland_client::{globals::registry_queue_init, Connection, QueueHandle};

struct AppData {
    registry_state: RegistryState,
    output_state: OutputState,
}

fn main() {
    // Connect to the compositor (reads WAYLAND_DISPLAY env var).
    let conn = Connection::connect_to_env().expect("failed to connect to Wayland compositor");

    // Ask the compositor what globals (capabilities) it exposes.
    let (globals, mut event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    let mut app_data = AppData {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
    };

    // Block until the compositor has told us about all current outputs.
    event_queue.roundtrip(&mut app_data).unwrap();

    println!("Connected outputs:");
    for output in app_data.output_state.outputs() {
        if let Some(info) = app_data.output_state.info(&output) {
            println!(
                "  {} ({}x{})",
                info.name.unwrap_or_default(),
                info.logical_size.map(|s| s.0).unwrap_or(0),
                info.logical_size.map(|s| s.1).unwrap_or(0)
            );
        }
    }
}

impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _output: wayland_client::protocol::wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _output: wayland_client::protocol::wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _output: wayland_client::protocol::wl_output::WlOutput) {}
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_registry!(AppData);

smithay_client_toolkit::delegate_dispatch2!(AppData);
