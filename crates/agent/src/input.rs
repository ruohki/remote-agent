//! Mouse and keyboard injection for [`protocol::channel::InputEvent`].
//!
//! TODO(builder-core): implement `Injector` on top of `enigo`:
//! * translate W3C `KeyboardEvent.code` (e.g. `KeyA`, `Digit1`, `ShiftLeft`, `MetaLeft`,
//!   `ArrowUp`, `F5`, `Numpad1`) to `enigo::Key` (unicode for letters/digits so the
//!   *layout of the remote machine* is respected, named keys otherwise);
//! * `MouseMove` receives physical pixels of the selected display → add the display's
//!   origin and divide by its `scale` to obtain global logical coordinates;
//! * track pressed keys/buttons so `ReleaseAll` (and session teardown) can release them;
//! * `Text` → `enigo.text()`; wheel lines → `enigo.scroll()`;
//! * reject everything when `AgentConfig.allow_input == false`.
//! * On macOS the process needs the Accessibility permission (`AXIsProcessTrusted`).

use anyhow::Result;
use protocol::channel::InputEvent;
use protocol::common::DisplayInfo;

pub struct Injector;

impl Injector {
    pub fn new() -> Result<Self> {
        anyhow::bail!("input injector not implemented yet")
    }

    pub fn set_display(&mut self, _display: &DisplayInfo) {}

    pub fn handle(&mut self, _event: InputEvent) -> Result<()> {
        Ok(())
    }

    pub fn release_all(&mut self) {}
}
