//! The performance overlay (Horizon's Overlay setting): frames per second,
//! frame, CPU and GPU times, and a graph of recent frame times.
use crate::{
    graphics::{Overlay, OverlayCorner},
    hud::{HudAtlas, HudBatch, rgba},
    text::Text,
};
use std::collections::VecDeque;

const HISTORY: usize = 160;

#[derive(Default)]
pub struct FrameGraph {
    /// Frame, CPU and GPU milliseconds of recent frames, oldest first.
    frames: VecDeque<(f32, f32, f32)>,
}
impl FrameGraph {
    pub fn record(&mut self, frame_ms: f32, cpu_ms: f32, gpu_ms: Option<f32>) {
        let gpu = gpu_ms.unwrap_or_else(|| self.frames.back().map_or(0.0, |f| f.2));
        if self.frames.len() == HISTORY {
            self.frames.pop_front();
        }
        self.frames.push_back((frame_ms, cpu_ms, gpu));
    }
    pub fn draw(
        &self,
        mode: Overlay,
        corner: OverlayCorner,
        scale_percent: u32,
        safe: [f32; 4],
        batch: &mut HudBatch,
        atlas: &HudAtlas,
    ) {
        if mode == Overlay::Off || self.frames.is_empty() {
            return;
        }
        let n = self.frames.len() as f32;
        let avg = |f: fn(&(f32, f32, f32)) -> f32| self.frames.iter().map(f).sum::<f32>() / n;
        let (frame, cpu, gpu) = (avg(|f| f.0), avg(|f| f.1), avg(|f| f.2));
        let worst = self.frames.iter().map(|f| f.0).fold(0.0f32, f32::max);
        let fps = 1000.0 / frame.max(0.001);

        let start = batch.vertices.len();
        let (box_w, box_h) = if mode == Overlay::Simple {
            let label = format!("{fps:.0} FPS");
            let width = (atlas.font.width(&label) + 8.0).max(40.0);
            let height = 14.0;
            batch.fill(atlas, 0.0, 0.0, width, height, rgba(0, 0, 0, 144));
            batch.text_styled(
                atlas,
                &Text::literal(label),
                4.0,
                3.0,
                rgba(255, 255, 255, 255),
                true,
            );
            (width, height)
        } else {
            let lines = [
                format!("{fps:.0} fps"),
                format!("frame {frame:.1} ms  max {worst:.1}"),
                format!("cpu {cpu:.1}  gpu {gpu:.1} ms"),
            ];
            let width = 120.0f32.max(
                lines
                    .iter()
                    .map(|l| atlas.font.width(l))
                    .fold(0.0, f32::max)
                    + 6.0,
            );
            let graph = if mode == Overlay::Graph { 42.0 } else { 0.0 };
            let height = 4.0 + lines.len() as f32 * 10.0 + graph;
            batch.fill(atlas, 0.0, 0.0, width, height, rgba(0, 0, 0, 144));
            for (i, line) in lines.iter().enumerate() {
                let color = if i == 0 {
                    rgba(255, 255, 255, 255)
                } else {
                    rgba(200, 200, 200, 255)
                };
                batch.text_styled(
                    atlas,
                    &Text::literal(line.clone()),
                    3.0,
                    2.0 + i as f32 * 10.0,
                    color,
                    true,
                );
            }
            if mode == Overlay::Graph {
                // Bars of frame time on a 0..33 ms scale; lines at 60 and 30 fps.
                let (gy, gh) = (2.0 + lines.len() as f32 * 10.0 + 2.0, 36.0);
                let scale = gh / 33.3;
                let bar = (width - 6.0) / HISTORY as f32;
                for (i, &(ms, cpu, _)) in self.frames.iter().enumerate() {
                    let height = (ms * scale).min(gh);
                    let bx = 3.0 + i as f32 * bar;
                    let color = if ms > 33.3 {
                        rgba(240, 90, 80, 255)
                    } else if ms > 16.7 {
                        rgba(240, 200, 90, 255)
                    } else {
                        rgba(110, 210, 120, 255)
                    };
                    batch.fill(atlas, bx, gy + gh - height, bar.max(1.0), height, color);
                    let cpu_height = (cpu * scale).min(height);
                    batch.fill(
                        atlas,
                        bx,
                        gy + gh - cpu_height,
                        bar.max(1.0),
                        cpu_height,
                        rgba(80, 140, 230, 200),
                    );
                }
                for target in [16.7, 33.3] {
                    let ly = gy + gh - target * scale;
                    batch.fill(
                        atlas,
                        3.0,
                        ly,
                        width - 6.0,
                        1.0,
                        rgba(255, 255, 255, 90),
                    );
                }
            }
            (width, height)
        };

        let s = (scale_percent.clamp(50, 200) as f32) / 100.0;
        let scaled_w = box_w * s;
        let scaled_h = box_h * s;
        let [sx, sy, sw, sh] = safe;
        let margin = 4.0;
        let (raw_x, raw_y) = match corner {
            OverlayCorner::TopLeft => (sx + margin, sy + margin),
            OverlayCorner::TopRight => (sx + sw - scaled_w - margin, sy + margin),
            OverlayCorner::BottomLeft => (sx + margin, sy + sh - scaled_h - margin),
            OverlayCorner::BottomRight => {
                (sx + sw - scaled_w - margin, sy + sh - scaled_h - margin)
            }
        };
        let ox = raw_x.clamp(0.0, (batch.width - scaled_w).max(0.0));
        let oy = raw_y.clamp(0.0, (batch.height - scaled_h).max(0.0));
        for v in &mut batch.vertices[start..] {
            v.pos[0] = ox + v.pos[0] * s;
            v.pos[1] = oy + v.pos[1] * s;
        }
    }
}
