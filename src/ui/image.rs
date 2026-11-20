//! Inline images: photo/sticker downloads decoded into terminal graphics
//! (kitty / iTerm2 / sixel / halfblocks via ratatui-image).

use std::collections::HashMap;

use ratatui::layout::Size;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FontSize, Resize};
use tdlib_rs::types::{Photo, PhotoSize};

/// An image's state, with the cell size it was requested or encoded at.
pub enum Entry {
    Requested {
        cols: u16,
        rows: u16,
    },
    /// Encoded at `cols × rows`. `resizing` is a newer size being encoded;
    /// the old image stays on screen meanwhile.
    Ready {
        proto: Protocol,
        cols: u16,
        rows: u16,
        resizing: Option<(u16, u16)>,
    },
    Failed,
}

pub struct Images {
    pub picker: Option<Picker>,
    pub max_cols: u16,
    pub max_rows: u16,
    pub entries: HashMap<i32, Entry>,
}

/// A file the message list wants shown, at a fixed cell size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted {
    pub file_id: i32,
    pub cols: u16,
    pub rows: u16,
}

impl Images {
    pub fn new(cfg: &crate::config::Images) -> Self {
        let picker = if cfg.enabled {
            match Picker::from_query_stdio() {
                Ok(mut p) => {
                    match cfg.protocol.to_ascii_lowercase().as_str() {
                        "kitty" => p.set_protocol_type(ProtocolType::Kitty),
                        "iterm2" => p.set_protocol_type(ProtocolType::Iterm2),
                        "sixel" => p.set_protocol_type(ProtocolType::Sixel),
                        "halfblocks" => p.set_protocol_type(ProtocolType::Halfblocks),
                        _ => {}
                    }
                    tracing::info!(
                        "image protocol {:?}, font {:?}",
                        p.protocol_type(),
                        p.font_size()
                    );
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!("image picker query failed ({e}); using halfblocks");
                    Some(Picker::halfblocks())
                }
            }
        } else {
            None
        };
        if picker.is_some() {
            unblock_query_thread();
        }
        Self {
            picker,
            max_cols: cfg.max_width.max(4),
            max_rows: cfg.max_height.max(2),
            entries: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            picker: None,
            max_cols: 40,
            max_rows: 12,
            entries: HashMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    fn font(&self) -> FontSize {
        self.picker
            .as_ref()
            .map(|p| p.font_size())
            .unwrap_or(FontSize {
                width: 8,
                height: 16,
            })
    }

    /// Cell box that keeps the aspect ratio inside `max_cols × max_rows`.
    pub fn cells_for(&self, px_w: i32, px_h: i32, max_cols: u16, max_rows: u16) -> (u16, u16) {
        fit_cells(
            px_w,
            px_h,
            self.font(),
            max_cols.min(self.max_cols),
            max_rows.min(self.max_rows),
        )
    }

    /// Smallest photo size at least as wide as the box, else the largest.
    pub fn pick_size<'a>(&self, photo: &'a Photo, cols: u16) -> Option<&'a PhotoSize> {
        let want_px = cols as i32 * self.font().width as i32;
        let mut sizes: Vec<&PhotoSize> = photo.sizes.iter().collect();
        sizes.sort_by_key(|s| s.width);
        sizes
            .iter()
            .find(|s| s.width >= want_px)
            .copied()
            .or_else(|| sizes.last().copied())
    }

    pub fn protocol(&self, file_id: i32) -> Option<&Protocol> {
        match self.entries.get(&file_id) {
            Some(Entry::Ready { proto, .. }) => Some(proto),
            _ => None,
        }
    }

    /// Whether `w` must be (re)requested: never asked for, or wanted at a
    /// size other than the one shown or on its way (the pane was resized).
    pub fn needs(&self, w: &Wanted) -> bool {
        let want = (w.cols, w.rows);
        match self.entries.get(&w.file_id) {
            None => true,
            Some(Entry::Failed) => false,
            Some(Entry::Requested { cols, rows }) => (*cols, *rows) != want,
            Some(Entry::Ready {
                cols,
                rows,
                resizing,
                ..
            }) => (*cols, *rows) != want && *resizing != Some(want),
        }
    }

    /// Records a request for `w`, keeping an encoded image while it resizes.
    pub fn request(&mut self, w: &Wanted) {
        let size = (w.cols, w.rows);
        match self.entries.get_mut(&w.file_id) {
            Some(Entry::Ready { resizing, .. }) => *resizing = Some(size),
            _ => {
                self.entries.insert(
                    w.file_id,
                    Entry::Requested {
                        cols: w.cols,
                        rows: w.rows,
                    },
                );
            }
        }
    }

    /// The size of the newest encode on its way, if any.
    pub fn latest_request(&self, file_id: i32) -> Option<(u16, u16)> {
        match self.entries.get(&file_id)? {
            Entry::Requested { cols, rows } => Some((*cols, *rows)),
            Entry::Ready { resizing, .. } => *resizing,
            Entry::Failed => None,
        }
    }

    pub fn failed(&self, file_id: i32) -> bool {
        matches!(self.entries.get(&file_id), Some(Entry::Failed))
    }
}

/// When the terminal ignores ratatui-image's capability query (tmux does),
/// the query gives up but its reader thread stays blocked on stdin and eats
/// the next keystrokes. A Device Status Report request makes every terminal
/// answer, which ends that read; if no thread is waiting, crossterm drops the
/// unknown reply.
fn unblock_query_thread() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[5n");
    let _ = out.flush();
    std::thread::sleep(std::time::Duration::from_millis(100));
}

pub fn fit_cells(px_w: i32, px_h: i32, font: FontSize, max_cols: u16, max_rows: u16) -> (u16, u16) {
    let (fw, fh) = (font.width.max(1) as f64, font.height.max(1) as f64);
    let (w, h) = (px_w.max(1) as f64, px_h.max(1) as f64);
    // Natural size in cells, then scale down to fit both limits.
    let nat_cols = w / fw;
    let nat_rows = h / fh;
    let scale = (max_cols as f64 / nat_cols)
        .min(max_rows as f64 / nat_rows)
        .min(1.0);
    let cols = (nat_cols * scale).round().clamp(1.0, max_cols as f64) as u16;
    let rows = (nat_rows * scale).round().clamp(1.0, max_rows as f64) as u16;
    (cols, rows)
}

/// Decodes and encodes an image for the terminal. Blocking: call from a worker.
pub fn encode(picker: &Picker, path: &str, cols: u16, rows: u16) -> anyhow::Result<Protocol> {
    let img = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?;
    let proto = picker.new_protocol(img, Size::new(cols, rows), Resize::Fit(None))?;
    Ok(proto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_inside_box() {
        let font = FontSize {
            width: 10,
            height: 20,
        };
        // 800x600 px = 80x30 cells natural; box 40x12 → limited by rows: scale 0.4 → 32x12.
        assert_eq!(fit_cells(800, 600, font, 40, 12), (32, 12));
        // Tall image limited by rows.
        assert_eq!(fit_cells(100, 1000, font, 40, 10), (2, 10));
        // Small image is not upscaled.
        assert_eq!(fit_cells(50, 40, font, 40, 12), (5, 2));
    }
}
