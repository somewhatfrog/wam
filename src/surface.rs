use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::window::Window;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};

pub struct Surface {
    pub window: Window,
    pub buffer: Option<Buffer>,
    pub scaled: Option<Buffer>,
    pub width: u32,
    pub height: u32,
    pub has_configure: bool,
}

impl Surface {
    pub fn set_buffer(&mut self, buffer: Buffer, width: u32, height: u32) {
        self.buffer = Some(buffer);
        self.width = width;
        self.height = height;
    }

    pub fn set_capture_buffer(&mut self, buffer: Buffer) {
        self.buffer = Some(buffer);
        self.scaled = None;
    }

    pub fn set_display_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    pub fn canvas<'a>(&self, pool: &'a mut SlotPool) -> Option<&'a mut [u8]> {
        self.buffer.as_ref().and_then(|b| b.canvas(pool))
    }

    pub fn present(&mut self) {
        if !self.has_configure {
            return;
        }
        let Some(buffer) = self.scaled.as_ref().or(self.buffer.as_ref()) else {
            return;
        };
        let surface = self.window.wl_surface();
        let _ = buffer.attach_to(surface);
        surface.damage(0, 0, self.width as i32, self.height as i32);
        self.window.commit();
    }
}