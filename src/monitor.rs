use smithay_client_toolkit::shell::xdg::window::Window;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use wayland_client::protocol::wl_shm;
use wayland_client::{QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

use crate::surface::Surface;
use crate::text::{Font, TEXT_NORMAL};
use crate::Wam;

const BLACK: [u8; 4] = [0, 0, 0, 255];

pub const DEFAULT_W: u32 = 960;
pub const DEFAULT_H: u32 = 540;

const SIZE_DIVISOR: u32 = 4;

pub enum SessionAction {
    None,
    Allocate {
        width: u32,
        height: u32,
        format: wl_shm::Format,
    },
    Stopped,
}

pub enum FrameAction {
    None,
    Ready,
    Retry,
    Reallocate,
    Stopped,
}

pub struct Monitor {
    pub surface: Surface,
    pub monitored_handle: ExtForeignToplevelHandleV1,
    session: Option<ExtImageCopyCaptureSessionV1>,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    pub capture_size: (u32, u32),
    capture_format: wl_shm::Format,
    scaled_size: (u32, u32),
    pending_size: Option<(u32, u32)>,
    pending_format: Option<wl_shm::Format>,
    status: Option<String>,
    stopped: bool,
}

impl Monitor {
    pub fn new(
        window: Window,
        monitored_handle: ExtForeignToplevelHandleV1,
        session: ExtImageCopyCaptureSessionV1,
    ) -> Self {
        Self {
            surface: Surface {
                window,
                buffer: None,
                scaled: None,
                width: DEFAULT_W,
                height: DEFAULT_H,
                has_configure: false,
            },
            monitored_handle,
            session: Some(session),
            frame: None,
            capture_size: (0, 0),
            capture_format: wl_shm::Format::Xrgb8888,
            scaled_size: (0, 0),
            pending_size: None,
            pending_format: None,
            status: Some("waiting for capture…".to_string()),
            stopped: false,
        }
    }

    pub fn on_session_event(
        &mut self,
        event: ext_image_copy_capture_session_v1::Event,
    ) -> SessionAction {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                self.pending_size = Some((width, height));
                SessionAction::None
            }
            Event::ShmFormat { format } => {
                if let WEnum::Value(format) = format {
                    if self.pending_format.is_none_or(|p| p != wl_shm::Format::Argb8888) {
                        self.pending_format = Some(format);
                    }
                }
                SessionAction::None
            }
            Event::Done => {
                let Some((width, height)) = self.pending_size else {
                    return SessionAction::None;
                };
                match self.pending_format {
                    Some(format) => SessionAction::Allocate { width, height, format },
                    None => {
                        eprintln!("wam: capture session advertised no usable shm format");
                        self.status = Some("capture not supported".to_string());
                        SessionAction::None
                    }
                }
            }
            Event::Stopped => {
                self.mark_stopped();
                SessionAction::Stopped
            }
            _ => SessionAction::None,
        }
    }

    pub fn on_frame_event(
        &mut self,
        event: ext_image_copy_capture_frame_v1::Event,
    ) -> FrameAction {
        use ext_image_copy_capture_frame_v1::{Event, FailureReason};
        match event {
            Event::Ready => {
                self.frame = None;
                FrameAction::Ready
            }
            Event::Failed { reason } => {
                self.frame = None;
                match reason {
                    WEnum::Value(FailureReason::BufferConstraints) => {
                        FrameAction::Reallocate
                    }
                    WEnum::Value(FailureReason::Stopped) => {
                        self.mark_stopped();
                        FrameAction::Stopped
                    }
                    _ => FrameAction::Retry,
                }
            }
            _ => FrameAction::None,
        }
    }

    pub fn has_session(&self, session: &ExtImageCopyCaptureSessionV1) -> bool {
        self.session.as_ref() == Some(session)
    }

    pub fn has_frame(&self, frame: &ExtImageCopyCaptureFrameV1) -> bool {
        self.frame.as_ref() == Some(frame)
    }

    pub fn set_capture_buffer(
        &mut self,
        buffer: Buffer,
        size: (u32, u32),
        format: wl_shm::Format,
    ) {
        let gw = (size.0 / SIZE_DIVISOR).max(1);
        let gh = (size.1 / SIZE_DIVISOR).max(1);
        self.surface.window.set_min_size(Some((gw, gh)));
        self.surface.window.set_max_size(Some((gw, gh)));
        self.capture_size = size;
        self.capture_format = format;
        self.surface.set_capture_buffer(buffer);
        self.scaled_size = (0, 0);
        if self.surface.width != gw || self.surface.height != gh {
            self.surface.set_display_size(gw, gh);
            self.surface.present();
        }
    }

    pub fn present_capture(&mut self, pool: &mut SlotPool) {
        self.status = None;
        if self.capture_size == (0, 0) || self.surface.buffer.is_none() {
            self.surface.present();
            return;
        }
        let (sw, sh) = self.capture_size;
        let (dw, dh) = (self.surface.width, self.surface.height);
        if dw == 0 || dh == 0 {
            return;
        }
        if sw == dw && sh == dh {
            self.surface.scaled = None;
            self.surface.present();
            return;
        }
        let (rw, rh) = fit(sw, sh, dw, dh);
        if rw == 0 || rh == 0 {
            return;
        }
        let need = (sw as usize) * (sh as usize) * 4;
        if !self.ensure_scaled_buffer(pool, dw, dh) {
            self.surface.set_display_size(dw, dh);
            self.surface.present();
            return;
        }
        // All pool growth (which re-mmaps the backing file) is done; taking both slot views
        // through raw pointers is safe because distinct slots are disjoint regions of one
        // mmap and no pool mutation may follow while they are live.
        let dst_ptr = {
            let slot = self.surface.scaled.as_ref().unwrap().slot();
            pool.raw_data_mut(&slot).as_mut_ptr()
        };
        let src_ptr = {
            let Some(src) = self.surface.buffer.as_ref().and_then(|b| b.canvas(pool)) else {
                self.surface.set_display_size(dw, dh);
                self.surface.present();
                return;
            };
            if src.len() < need {
                self.surface.set_display_size(dw, dh);
                self.surface.present();
                return;
            }
            src.as_ptr()
        };
        let dst = unsafe { std::slice::from_raw_parts_mut(dst_ptr, (dw * dh * 4) as usize) };
        let src = unsafe { std::slice::from_raw_parts(src_ptr, need) };
        let (x0, y0) = ((dw - rw) / 2, (dh - rh) / 2);
        scale_letterbox(src, dst, sw, sh, dw, x0, y0, rw, rh);
        self.surface.set_display_size(dw, dh);
        self.surface.present();
    }

    fn ensure_scaled_buffer(&mut self, pool: &mut SlotPool, dw: u32, dh: u32) -> bool {
        if self.scaled_size != (dw, dh) || self.surface.scaled.is_none() {
            self.surface.scaled = None;
            let Ok((buffer, _canvas)) =
                pool.create_buffer(dw as i32, dh as i32, (dw * 4) as i32, self.capture_format)
            else {
                eprintln!("wam: failed to allocate scaled buffer");
                return false;
            };
            self.surface.scaled = Some(buffer);
            self.scaled_size = (dw, dh);
        }
        true
    }

    pub fn take_pending_constraints(&mut self) -> Option<(u32, u32, wl_shm::Format)> {
        let (width, height) = self.pending_size?;
        self.pending_format.map(|format| (width, height, format))
    }

    pub fn request_capture(&mut self, qh: &QueueHandle<Wam>) {
        if self.frame.is_some() || self.stopped {
            return;
        }
        let (Some(session), Some(buffer)) =
            (self.session.as_ref(), self.surface.buffer.as_ref())
        else {
            return;
        };
        let (w, h) = self.capture_size;
        if w == 0 || h == 0 {
            return;
        }
        let frame = session.create_frame(qh, ());
        frame.attach_buffer(buffer.wl_buffer());
        frame.damage_buffer(0, 0, w as i32, h as i32);
        frame.capture();
        self.frame = Some(frame);
    }

    fn mark_stopped(&mut self) {
        self.stopped = true;
        self.status = Some("source closed".to_string());
        self.frame = None;
        self.session = None;
    }

    pub fn redraw(&mut self, pool: &mut SlotPool, font: &Font) {
        let Some(status) = &self.status else {
            return;
        };
        if self.capture_size.0 != 0 {
            return;
        }
        let w = self.surface.width;
        let h = self.surface.height;
        let Some(canvas) = self.surface.canvas(pool) else {
            return;
        };
        for chunk in canvas.as_chunks_mut::<4>().0 {
            *chunk = BLACK;
        }
        let size = 14.0f32;
        let tw = font.measure(status, size) as i32;
        let x = ((w as i32 - tw) / 2).max(8);
        font.draw(canvas, w, h, x, (h / 2) as i32 + 5, status, size, TEXT_NORMAL);
        self.surface.present();
    }
}

fn fit(sw: u32, sh: u32, dw: u32, dh: u32) -> (u32, u32) {
    let (sw, sh, dw, dh) = (sw as u64, sh as u64, dw as u64, dh as u64);
    if dw * sh <= dh * sw {
        (dw as u32, (sh * dw / sw) as u32)
    } else {
        ((sw * dh / sh) as u32, dh as u32)
    }
}

fn scale_letterbox(
    src: &[u8],
    dst: &mut [u8],
    sw: u32,
    sh: u32,
    dw: u32,
    x0: u32,
    y0: u32,
    rw: u32,
    rh: u32,
) {
    let (sw, sh) = (sw as usize, sh as usize);
    let (dw, x0, y0, rw, rh) = (dw as usize, x0 as usize, y0 as usize, rw as usize, rh as usize);
    for row in dst.chunks_exact_mut(dw * 4) {
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&BLACK);
        }
    }
    let xmap: Vec<usize> = (0..rw).map(|x| x * sw / rw).collect();
    for (dy, row) in dst
        .chunks_exact_mut(dw * 4)
        .enumerate()
        .skip(y0)
        .take(rh)
    {
        let sy = ((dy - y0) * sh / rh) * sw * 4;
        for (px, sx) in row
            .chunks_exact_mut(4)
            .skip(x0)
            .take(rw)
            .zip(xmap.iter())
        {
            let i = sy + sx * 4;
            px.copy_from_slice(&src[i..i + 4]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fit, scale_letterbox, BLACK};

    fn src4x4() -> Vec<u8> {
        (0..4)
            .flat_map(|y| (0..4).flat_map(move |x| [x as u8, y as u8, 0, 255]))
            .collect()
    }

    fn black(p: &[u8]) -> bool {
        p == &BLACK
    }

    #[test]
    fn fit_preserves_aspect() {
        assert_eq!(fit(4, 4, 8, 8), (8, 8));
        assert_eq!(fit(8, 4, 8, 8), (8, 4));
        assert_eq!(fit(4, 8, 8, 8), (4, 8));
        assert_eq!(fit(4, 2, 10, 10), (10, 5));
        assert_eq!(fit(4, 8, 10, 10), (5, 10));
    }

    #[test]
    fn letterbox_fills_bars_and_centers() {
        let src = src4x4();

        let mut dst = vec![0u8; 8 * 8 * 4];
        scale_letterbox(&src, &mut dst, 4, 4, 8, 0, 0, 8, 8);
        assert_eq!(&dst[0..4], &[0, 0, 0, 255]);
        assert_eq!(&dst[8 * 7 * 4 + 7 * 4..][..4], &[3, 3, 0, 255]);

        let (rw, rh) = fit(4, 2, 10, 10);
        assert_eq!((rw, rh), (10, 5));
        let (x0, y0) = ((10 - rw) / 2, (10 - rh) / 2);
        let (x0, y0) = (x0 as usize, y0 as usize);
        let mut wide = vec![0u8; 10 * 10 * 4];
        scale_letterbox(&src, &mut wide, 4, 2, 10, x0 as u32, y0 as u32, rw, rh);
        let topbar = &wide[..10 * 4];
        assert!(topbar.chunks(4).all(black));
        let bottombar = &wide[9 * 10 * 4..];
        assert!(bottombar.chunks(4).all(black));
        assert_eq!(&wide[y0 * 10 * 4 + 0 * 4..y0 * 10 * 4 + 4], &[0, 0, 0, 255]);
        assert_eq!(&wide[y0 * 10 * 4 + 9 * 4..y0 * 10 * 4 + 9 * 4 + 4], &[3, 0, 0, 255]);
    }

    #[test]
    fn letterbox_with_offsets_maps_rows_inside_source() {
        let src: Vec<u8> = (0..2)
            .flat_map(|y| (0..4).flat_map(move |x| [x as u8, y as u8, 0, 255]))
            .collect();
        let (rw, rh) = fit(4, 2, 8, 6);
        assert_eq!((rw, rh), (8, 4));
        let (x0, y0) = ((8 - rw) / 2, (6 - rh) / 2);
        assert_eq!((x0, y0), (0, 1));
        let mut dst = vec![0u8; 8 * 6 * 4];
        scale_letterbox(&src, &mut dst, 4, 2, 8, x0, y0, rw, rh);
        assert!(dst[..8 * 4].chunks(4).all(black));
        assert_eq!(&dst[5 * 8 * 4..5 * 8 * 4 + 4], &BLACK);
        assert_eq!(&dst[1 * 8 * 4..1 * 8 * 4 + 4], &[0, 0, 0, 255]);
        assert_eq!(&dst[1 * 8 * 4 + 7 * 4..1 * 8 * 4 + 7 * 4 + 4], &[3, 0, 0, 255]);
        assert_eq!(&dst[4 * 8 * 4..4 * 8 * 4 + 4], &[0, 1, 0, 255]);
        assert_eq!(&dst[4 * 8 * 4 + 7 * 4..4 * 8 * 4 + 7 * 4 + 4], &[3, 1, 0, 255]);
    }
}
