use std::cmp::Ordering;

use smithay_client_toolkit::shell::xdg::window::Window;
use smithay_client_toolkit::shm::slot::SlotPool;
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;

use crate::surface::Surface;
use crate::text::{fill_rect, Font, TEXT_NORMAL};

const BG: u32 = 0xFF1E1F21;
const ROW_SELECTED: u32 = 0xFF373B41;
const TEXT_BRIGHT: u32 = 0xFFFFFFFF;
const TEXT_DIM: u32 = 0xFF969896;

pub const MAX_ROWS: u32 = 10;
pub const TITLE_CHARS: u32 = 24;
pub const APP_ID_CHARS: u32 = 12;
pub const MARGIN: f32 = TITLE_SIZE / 2.0;
pub const TITLE_SIZE: f32 = 15.0;
pub const APP_ID_SIZE: f32 = 13.0;

pub fn row_height(font: &Font) -> u32 {
    (font.line_height(TITLE_SIZE) + 2.0 * MARGIN * font.scale).ceil() as u32
}

fn col_widths(font: &Font) -> (f32, f32) {
    (
        font.measure("M", TITLE_SIZE) * TITLE_CHARS as f32,
        font.measure("M", APP_ID_SIZE) * APP_ID_CHARS as f32,
    )
}

pub fn size(font: &Font) -> (u32, u32) {
    let (title_w, app_id_w) = col_widths(font);
    let w = (MARGIN + title_w + MARGIN + app_id_w + MARGIN).ceil() as u32;
    (w, row_height(font) * MAX_ROWS)
}

#[derive(Clone)]
pub struct AppEntry {
    pub handle: ExtForeignToplevelHandleV1,
    pub title: String,
    pub app_id: String,
}

pub struct Prompt {
    pub surface: Surface,
    pub apps: Vec<AppEntry>,
    pub selected: usize,
    scroll: usize,
}

impl Prompt {
    pub fn new(window: Window, font: &Font) -> Self {
        let (w, h) = size(font);
        Self {
            surface: Surface {
                window,
                buffer: None,
                scaled: None,
                width: w,
                height: h,
                has_configure: false,
            },
            apps: Vec::new(),
            selected: 0,
            scroll: 0,
        }
    }

    pub fn add_app(&mut self, entry: AppEntry) {
        self.apps.push(entry);
        self.resort();
    }

    pub fn update_app(&mut self, handle: &ExtForeignToplevelHandleV1, title: String, app_id: String) {
        if let Some(entry) = self.apps.iter_mut().find(|e| e.handle == *handle) {
            entry.title = title;
            entry.app_id = app_id;
            self.resort();
        }
    }

    pub fn remove_app(&mut self, handle: &ExtForeignToplevelHandleV1) {
        self.apps.retain(|e| e.handle != *handle);
        self.resort();
    }

    fn resort(&mut self) {
        let selected_handle = self.apps.get(self.selected).map(|e| e.handle.clone());
        self.apps.sort_by(cmp_entries);
        if let Some(handle) = selected_handle
            && let Some(pos) = self.apps.iter().position(|e| e.handle == handle)
        {
            self.selected = pos;
        }
        self.clamp_selection();
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.apps.is_empty() {
            return;
        }
        let len = self.apps.len() as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(len)) as usize;
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.selected >= self.apps.len() {
            self.selected = self.apps.len().saturating_sub(1);
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll + MAX_ROWS as usize {
            self.scroll = self.selected - MAX_ROWS as usize + 1;
        }
    }

    pub fn redraw(&mut self, pool: &mut SlotPool, font: &Font) {
        let Some(canvas) = self.surface.canvas(pool) else {
            return;
        };
        self.render(canvas, font);
        self.surface.present();
    }

    fn render(&self, canvas: &mut [u8], font: &Font) {
        let w = self.surface.width;
        let h = self.surface.height;
        let scale = font.scale;
        let margin = (MARGIN * scale) as u32;
        let row_h = row_height(font);
        let (title_w, app_id_w) = col_widths(font);
        let title_base = (row_h as f32 - font.line_height(TITLE_SIZE)) / 2.0 + font.ascent(TITLE_SIZE);
        let app_id_base = (row_h as f32 - font.line_height(APP_ID_SIZE)) / 2.0 + font.ascent(APP_ID_SIZE);

        fill_rect(canvas, w, h, 0, 0, w, h, BG);

        for i in 0..MAX_ROWS {
            let idx = self.scroll + i as usize;
            let Some(app) = self.apps.get(idx) else {
                break;
            };
            let y = row_h * i;
            let selected = idx == self.selected;
            if selected {
                fill_rect(canvas, w, h, 0, y, w, row_h, ROW_SELECTED);
            }
            let title = font.truncate(&app.title, TITLE_SIZE, title_w);
            font.draw(
                canvas,
                w,
                h,
                margin as i32,
                y as i32 + title_base as i32,
                &title,
                TITLE_SIZE,
                if selected { TEXT_BRIGHT } else { TEXT_NORMAL },
            );
            let app_id = font.truncate(&app.app_id, APP_ID_SIZE, app_id_w);
            let app_id_x = w
                .saturating_sub(margin)
                .saturating_sub(font.measure(&app_id, APP_ID_SIZE) as u32)
                .max(margin);
            font.draw(
                canvas,
                w,
                h,
                app_id_x as i32,
                y as i32 + app_id_base as i32,
                &app_id,
                APP_ID_SIZE,
                TEXT_DIM,
            );
        }
    }
}

fn cmp_entries(a: &AppEntry, b: &AppEntry) -> Ordering {
    let la = a.title.to_lowercase();
    let lb = b.title.to_lowercase();
    la.cmp(&lb).then_with(|| a.title.cmp(&b.title))
}