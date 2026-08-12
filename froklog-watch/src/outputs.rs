//! Monitor enumeration for the meter's output picker.
//!
//! layershellev can TARGET an output by name (`with_xdg_output_name`) but
//! offers no way to list them, so this opens a short-lived Wayland
//! connection of its own: bind every `wl_output` at version ≥ 4 (the
//! version that added the `name` event — "DP-1", "HDMI-A-1"), roundtrip,
//! collect, disconnect. Layer surfaces are invisible to the compositor's
//! window management (no workspace switcher, no move-to-monitor), so an
//! explicit picker is the only way a user can send the meter elsewhere.

use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};

#[derive(Default)]
struct Enumerator {
    /// (name "DP-1", description "Dell U2723QE …")
    outputs: Vec<(String, String)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Enumerator {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wl_output" && version >= 4 {
                let idx = state.outputs.len();
                state.outputs.push((String::new(), String::new()));
                registry.bind::<wl_output::WlOutput, _, _>(name, 4, qh, idx);
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for Enumerator {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(slot) = state.outputs.get_mut(*idx) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => slot.0 = name,
            wl_output::Event::Description { description } => slot.1 = description,
            _ => {}
        }
    }
}

/// Every connected monitor as (connector name, human description).
/// Empty on error or off-Wayland — callers fall back to "(focused monitor)".
pub fn list() -> Vec<(String, String)> {
    let Ok(conn) = Connection::connect_to_env() else {
        return Vec::new();
    };
    let display = conn.display();
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    display.get_registry(&qh, ());
    let mut state = Enumerator::default();
    // First roundtrip surfaces the globals (and our binds), the second
    // delivers each bound output's name/description events.
    for _ in 0..2 {
        if queue.roundtrip(&mut state).is_err() {
            return Vec::new();
        }
    }
    state.outputs.retain(|(n, _)| !n.is_empty());
    state.outputs
}
