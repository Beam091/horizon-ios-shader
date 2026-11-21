//! iOS controllers: gilrs has no iOS backend, so the pads GameController.framework lists
//! (`horizon_client_input::ios_controllers`) are read once per loop iteration into the same
//! `PadState` gilrs' pads give on other platforms.
use super::Connection;
use horizon_client::controller_ui::Device;
use horizon_client::gamepad::{GlyphStyle, PadState};
use horizon_client_input::ios_controllers;

/// The controller last seen, so a pad coming or going is reported once.
#[derive(Default)]
pub(super) struct Reader {
    present: Option<String>,
}

impl Reader {
    /// The state of the controller being used (the first with something pressed or tilted, else
    /// the first one), its name and button style; connections come and go into `connections`.
    pub(super) fn read(
        &mut self,
        connections: &mut Vec<Connection>,
        device: &mut Option<Device>,
        style: &mut GlyphStyle,
    ) -> PadState {
        let pads = ios_controllers::connected();
        let used = pads
            .iter()
            .find(|p| p.state != PadState::default())
            .or(pads.first());
        match (&self.present, used) {
            (None, Some(pad)) => {
                log::info!("Controller connected: {}", pad.name);
                connections.push(Connection::Connected(pad.style));
            }
            (Some(name), None) => {
                log::info!("Controller disconnected: {name}");
                connections.push(Connection::Disconnected { active: true });
            }
            _ => {}
        }
        self.present = used.map(|p| p.name.clone());
        match used {
            Some(pad) => {
                *device = Some(Device {
                    name: pad.name.clone(),
                    vendor: None,
                    product: None,
                });
                *style = pad.style;
                pad.state
            }
            None => {
                *device = None;
                PadState::default()
            }
        }
    }
}
