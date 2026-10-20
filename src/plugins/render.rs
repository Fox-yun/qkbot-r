//! Small CPU renderer: no browser process, HTML engine, or permanently loaded glyph atlas.
use super::native::NativeView;
use crate::{host::Host, types::OutgoingMessage};
use ab_glyph::{Font as _, FontArc, FontVec, point};
use anyhow::{Context, Result, ensure};
use chrono::Datelike;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    io::Cursor,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, OnceLock},
};

type Font = FontArc;
type FontCache = Mutex<Option<(PathBuf, Arc<Font>)>>;
static FONT: OnceLock<FontCache> = OnceLock::new();
fn font(configured: &str) -> Result<Arc<Font>> {
    let path = if !configured.is_empty() {
        PathBuf::from(configured)
    } else {
        [
            "C:/Windows/Fonts/msyh.ttc",
            "C:/Windows/Fonts/simhei.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/System/Library/Fonts/PingFang.ttc",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .context("未找到中文字体，请在原生插件配置中设置 image_font_path")?
    };
    let mut cache = FONT.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((saved, font)) = &*cache
        && saved == &path
    {
        return Ok(font.clone());
    }
    ensure!(
        std::fs::metadata(&path)?.len() <= 40 * 1024 * 1024,
        "字体文件过大"
    );
    let bytes = std::fs::read(&path)?;
    // TTC collections use face zero, matching the previous renderer. FontVec
    // parses font tables here; only requested glyphs get their outlines loaded.
    let loaded =
        FontVec::try_from_vec_and_index(bytes, 0).map_err(|_| anyhow::anyhow!("字体无法读取"))?;
    ensure!(
        loaded.units_per_em().is_some_and(|units| units > 0.) && loaded.height_unscaled() > 0.,
        "字体尺寸无效"
    );
    let loaded = Arc::new(FontArc::new(loaded));
    *cache = Some((path, loaded.clone()));
    Ok(loaded)
}

#[derive(Clone, Copy)]
struct Metrics {
    width: usize,
    height: usize,
    xmin: i32,
    ymin: i32,
    advance_width: f32,
}
struct Glyph {
    metrics: Metrics,
    bitmap: Vec<u8>,
}
fn advance(font: &Font, ch: char, size: f32) -> f32 {
    font.h_advance_unscaled(font.glyph_id(ch)) * size / font.units_per_em().unwrap_or(1.)
}
fn rasterize(font: &Font, ch: char, size: f32) -> Glyph {
    // ab_glyph scales by ascent-descent; Canvas sizes are pixels per em.
    // Converting explicitly preserves the existing layout and line wrapping.
    let scale = size * font.height_unscaled() / font.units_per_em().unwrap_or(1.);
    let glyph = font
        .glyph_id(ch)
        .with_scale_and_position(scale, point(0., 0.));
    let mut metrics = Metrics {
        width: 0,
        height: 0,
        xmin: 0,
        ymin: 0,
        advance_width: advance(font, ch, size),
    };
    let Some(outline) = font.outline_glyph(glyph) else {
        return Glyph {
            metrics,
            bitmap: Vec::new(),
        };
    };
    let bounds = outline.px_bounds();
    metrics.width = bounds.width() as usize;
    metrics.height = bounds.height() as usize;
    metrics.xmin = bounds.min.x as i32;
    metrics.ymin = -bounds.max.y as i32;
    let mut bitmap = vec![0; metrics.width * metrics.height];
    outline.draw(|x, y, coverage| {
        bitmap[y as usize * metrics.width + x as usize] = (coverage * 255.).round() as u8;
    });
    Glyph { metrics, bitmap }
}
// This cache belongs to one blocking render task, including all of its menu
// pages. Neither unusual text nor repeated jobs grow a persistent glyph atlas.
struct GlyphCache {
    entries: HashMap<(char, u32), Rc<Glyph>>,
    order: VecDeque<(char, u32)>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    #[cfg(test)]
    rasterizations: usize,
}
impl Default for GlyphCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_entries: 512,
            max_bytes: 1024 * 1024,
            #[cfg(test)]
            rasterizations: 0,
        }
    }
}
impl GlyphCache {
    fn get(&mut self, font: &Font, ch: char, size: f32) -> Rc<Glyph> {
        let key = (ch, size.to_bits());
        if let Some(glyph) = self.entries.get(&key) {
            return glyph.clone();
        }
        let glyph = Rc::new(rasterize(font, ch, size));
        #[cfg(test)]
        {
            self.rasterizations += 1;
        }
        if self.max_entries == 0 || glyph.bitmap.len() > self.max_bytes {
            return glyph;
        }
        while self.entries.len() >= self.max_entries
            || self.bytes + glyph.bitmap.len() > self.max_bytes
        {
            let old = self.order.pop_front().expect("nonempty glyph cache");
            if let Some(removed) = self.entries.remove(&old) {
                self.bytes -= removed.bitmap.len();
            }
        }
        self.bytes += glyph.bitmap.len();
        self.order.push_back(key);
        self.entries.insert(key, glyph.clone());
        glyph
    }
}
type TaskGlyphCache = Rc<RefCell<GlyphCache>>;

pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pixels: Vec<u8>,
    font: Arc<Font>,
    glyphs: TaskGlyphCache,
}
impl Canvas {
    pub fn new(width: u32, height: u32, font: Arc<Font>) -> Result<Self> {
        Self::with_glyphs(width, height, font, Rc::default())
    }
    fn with_glyphs(
        width: u32,
        height: u32,
        font: Arc<Font>,
        glyphs: TaskGlyphCache,
    ) -> Result<Self> {
        ensure!(
            width <= 1200 && height <= 8192 && width * height <= 8_000_000,
            "图片尺寸过大"
        );
        Ok(Self {
            width,
            height,
            pixels: vec![247; width as usize * height as usize * 3],
            font,
            glyphs,
        })
    }
    pub fn rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                let p = (yy * self.width + xx) as usize * 3;
                self.pixels[p..p + 3].copy_from_slice(&color);
            }
        }
    }
    pub fn text_height(&self, text: &str, size: f32, width: u32) -> u32 {
        let line = (size * 1.45).ceil() as u32;
        let mut lines = 1;
        let mut left = 0.;
        for ch in text.chars().take(10_000) {
            if ch == '\n' {
                lines += 1;
                left = 0.;
                continue;
            }
            if ch.is_control() {
                continue;
            }
            let advance = advance(&self.font, ch, size);
            if left + advance > width as f32 {
                lines += 1;
                left = 0.;
            }
            left += advance;
        }
        lines * line
    }
    pub fn text(
        &mut self,
        text: &str,
        x: u32,
        y: u32,
        size: f32,
        color: [u8; 3],
        width: u32,
    ) -> u32 {
        let mut left = x as f32;
        let mut top = y;
        let line = (size * 1.45).ceil() as u32;
        for ch in text.chars().take(10_000) {
            if ch == '\n' {
                left = x as f32;
                top += line;
                continue;
            }
            if ch.is_control() {
                continue;
            }
            let glyph = self.glyphs.borrow_mut().get(&self.font, ch, size);
            let metrics = glyph.metrics;
            if left + metrics.advance_width > (x + width) as f32 {
                left = x as f32;
                top += line;
            }
            if top + line > self.height {
                break;
            }
            let base_x = left as i32 + metrics.xmin;
            let base_y = top as i32 + size as i32 - metrics.height as i32 - metrics.ymin;
            for gy in 0..metrics.height {
                for gx in 0..metrics.width {
                    let xx = base_x + gx as i32;
                    let yy = base_y + gy as i32;
                    if xx < 0 || yy < 0 || xx >= self.width as i32 || yy >= self.height as i32 {
                        continue;
                    }
                    let alpha = glyph.bitmap[gy * metrics.width + gx] as u32;
                    let pixel = (yy as u32 * self.width + xx as u32) as usize * 3;
                    for (channel, &c) in color.iter().enumerate() {
                        self.pixels[pixel + channel] = ((self.pixels[pixel + channel] as u32
                            * (255 - alpha)
                            + c as u32 * alpha)
                            / 255) as u8;
                    }
                }
            }
            left += metrics.advance_width;
        }
        top + line
    }
    pub fn picture(
        &mut self,
        bytes: &[u8],
        x: u32,
        y: u32,
        max_width: u32,
        max_height: u32,
    ) -> Result<u32> {
        let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(32 * 1024 * 1024);
        reader.limits(limits);
        let decoded = reader.decode()?.thumbnail(max_width, max_height).to_rgb8();
        for (xx, yy, pixel) in decoded.enumerate_pixels() {
            if x + xx < self.width && y + yy < self.height {
                let p = ((y + yy) * self.width + x + xx) as usize * 3;
                self.pixels[p..p + 3].copy_from_slice(&pixel.0);
            }
        }
        Ok(decoded.height())
    }
    pub fn png(self, used_height: u32) -> Result<Vec<u8>> {
        let height = used_height.max(1).min(self.height);
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, self.width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            // Lossless fdeflate encoding avoids spending chat latency on the
            // default general-purpose DEFLATE compressor.
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&self.pixels[..(self.width * height * 3) as usize])?;
        }
        Ok(out)
    }
}
fn native_png(view: NativeView, font: Arc<Font>) -> Result<Vec<Vec<u8>>> {
    if let NativeView::Menu { kind, items } = &view {
        ensure!(items.len() <= 2000, "菜单最多2000项");
        let title = match kind.as_str() {
            "eat" => "菜品菜单",
            "drink" => "饮品菜单",
            _ => kind,
        };
        let mut pages = Vec::new();
        let mut index = 0;
        let glyphs = TaskGlyphCache::default();
        loop {
            let mut c = Canvas::with_glyphs(900, 4000, font.clone(), glyphs.clone())?;
            let page_title = format!("{title} · 第 {} 页", pages.len() + 1);
            let mut y = c.text(&page_title, 32, 24, 30., [25, 40, 55], 830) + 24;
            let first = index;
            while index < items.len() {
                let line = format!("{}. {}", index + 1, items[index]);
                let height = c.text_height(&line, 22., 830);
                if y + height + 30 > c.height {
                    break;
                }
                y = c.text(&line, 32, y, 22., [45, 60, 75], 830) + 10;
                index += 1;
            }
            ensure!(
                index > first || items.is_empty(),
                "单条菜单文字过长，无法排版"
            );
            if items.is_empty() {
                y = c.text("暂无条目", 32, y, 22., [90, 100, 110], 830) + 10;
            }
            pages.push(c.png(y + 20)?);
            if index == items.len() {
                return Ok(pages);
            }
        }
    }
    let mut c = Canvas::new(900, 2400, font)?;
    let end = match view {
        NativeView::Calendar {
            year,
            month,
            subject_id,
            name,
            days,
        } => {
            let title_end = c.text(
                &format!("{name} · {year}年{month}月"),
                32,
                24,
                30.,
                [25, 40, 55],
                830,
            );
            let identity_end = c.text(
                &format!("QQ {subject_id}"),
                32,
                title_end + 12,
                18.,
                [95, 105, 115],
                830,
            );
            let labels_y = identity_end + 22;
            for (i, label) in ["一", "二", "三", "四", "五", "六", "日"]
                .iter()
                .enumerate()
            {
                c.text(
                    label,
                    47 + i as u32 * 120,
                    labels_y,
                    22.,
                    [90, 100, 110],
                    100,
                );
            }
            let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).context("日期无效")?;
            let offset = first.weekday().num_days_from_monday();
            let mut end = labels_y;
            for day in 1..=31 {
                if chrono::NaiveDate::from_ymd_opt(year, month, day).is_none() {
                    break;
                }
                let index = offset + day - 1;
                let x = 30 + index % 7 * 120;
                let y = labels_y + 45 + index / 7 * 92;
                let count = days.iter().find(|(d, _)| *d == day).map_or(0, |(_, n)| *n);
                c.rect(
                    x,
                    y,
                    108,
                    80,
                    if count > 0 {
                        [210, 239, 224]
                    } else {
                        [235, 238, 242]
                    },
                );
                c.text(&day.to_string(), x + 10, y + 8, 24., [35, 50, 65], 90);
                if count > 0 {
                    let count = format!("×{count}");
                    let size = (160. / count.chars().count() as f32).min(18.);
                    c.text(&count, x + 10, y + 43, size, [25, 125, 80], 90);
                }
                end = y + 80;
            }
            end + 28
        }
        NativeView::Wordle {
            length,
            attempts,
            guesses,
            marks,
            hint,
        } => {
            ensure!(
                (3..=8).contains(&length) && attempts == length + 1 && guesses.len() <= attempts,
                "棋盘尺寸无效"
            );
            ensure!(
                guesses
                    .iter()
                    .all(|word| word.len() == length && word.is_ascii()),
                "棋盘单词格式无效"
            );
            c.text("Wordle", 32, 24, 32., [25, 40, 55], 830);
            let size = (800 / length as u32).min(82);
            for row in 0..attempts {
                for col in 0..length {
                    let x = 40 + col as u32 * (size + 8);
                    let y = 90 + row as u32 * (size + 8);
                    let mark = marks.get(row).and_then(|m| m.get(col)).copied();
                    let color = match mark {
                        Some(2) => [83, 150, 99],
                        Some(1) => [192, 163, 71],
                        Some(_) => [119, 128, 139],
                        None => [225, 230, 236],
                    };
                    c.rect(x, y, size, size, color);
                    if let Some(ch) = guesses.get(row).and_then(|g| g.chars().nth(col)) {
                        c.text(
                            &ch.to_ascii_uppercase().to_string(),
                            x + size / 3,
                            y + size / 5,
                            size as f32 * 0.55,
                            [255, 255, 255],
                            size,
                        );
                    }
                }
            }
            let mut end = 105 + attempts as u32 * (size + 8);
            if let Some(hint) = hint {
                end = c.text(&format!("提示：{hint}"), 32, end, 22., [50, 65, 80], 830) + 24;
            }
            end
        }
        NativeView::Rank { title, entries } => {
            ensure!(entries.len() <= 100, "排行榜条目过多");
            let mut y = c.text(&title, 32, 24, 30., [25, 40, 55], 830) + 24;
            for (i, (id, name, count)) in entries.iter().enumerate() {
                let text = format!("{}. {} ({}) · {} 次", i + 1, name, id, count);
                ensure!(
                    y + c.text_height(&text, 24., 830) + 24 < c.height,
                    "排行榜过长"
                );
                y = c.text(&text, 32, y, 24., [45, 60, 75], 830) + 16;
            }
            if entries.is_empty() {
                y = c.text("本月暂无记录", 32, y, 24., [90, 100, 110], 830);
            }
            y + 20
        }
        NativeView::Menu { .. } => unreachable!(),
    };
    Ok(vec![c.png(end)?])
}

pub async fn native(host: &Host, view: &NativeView) -> Result<OutgoingMessage> {
    let view = view.clone();
    let path = host.config.plugins.native.image_font_path.clone();
    let permit = host
        .media
        .rendering
        .clone()
        .try_acquire_owned()
        .context("绘图繁忙，请稍后重试")?;
    let pages = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        native_png(view, font(&path)?)
    })
    .await??;
    let mut segments = Vec::new();
    for bytes in pages {
        segments.extend(host.media.png(host, bytes).await?.segments);
    }
    Ok(OutgoingMessage { segments })
}
pub async fn lines(host: &Host, title: &str, lines: &[String]) -> Result<OutgoingMessage> {
    native(
        host,
        &NativeView::Menu {
            kind: title.into(),
            items: lines.to_vec(),
        },
    )
    .await
}

#[derive(Clone)]
pub struct ChatBubble {
    pub name: String,
    pub text: String,
    pub avatar: Option<Vec<u8>>,
    pub images: Vec<Vec<u8>>,
    pub is_bot: bool,
}
pub async fn chat(host: &Host, bubbles: Vec<ChatBubble>) -> Result<OutgoingMessage> {
    ensure!(
        !bubbles.is_empty() && bubbles.len() <= 24,
        "模拟聊天最多24条消息"
    );
    let path = host.config.plugins.native.image_font_path.clone();
    let permit = host
        .media
        .rendering
        .clone()
        .try_acquire_owned()
        .context("绘图繁忙")?;
    let bytes = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut c = Canvas::new(900, 8192, font(&path)?)?;
        c.rect(0, 0, 900, 65, [221, 232, 248]);
        c.text("模拟对话 · 非真实聊天记录", 30, 15, 26., [55, 83, 125], 840);
        let mut y = 90;
        for bubble in bubbles {
            ensure!(y < 7600, "模拟聊天图片过长");
            c.rect(30, y, 70, 70, [218, 225, 232]);
            if let Some(avatar) = bubble.avatar {
                let _ = c.picture(&avatar, 30, y, 70, 70);
            }
            c.text(
                &format!(
                    "{}{}",
                    if bubble.is_bot { "[BOT] " } else { "" },
                    bubble.name
                ),
                125,
                y,
                22.,
                [95, 107, 122],
                730,
            );
            let estimate = (bubble.text.chars().count() / 30 + 2) as u32 * 36 + 20;
            c.rect(120, y + 38, 730, estimate, [255, 255, 255]);
            let mut end = c
                .text(&bubble.text, 140, y + 52, 26., [30, 40, 52], 690)
                .max(y + 90);
            for image in bubble.images {
                let height = c.picture(&image, 140, end + 12, 600, 400)?;
                end += height + 20;
            }
            y = end + 40;
        }
        c.png(y + 20)
    })
    .await??;
    host.media.png(host, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn available_font() -> Option<Arc<Font>> {
        let configured = std::env::var("QKBOT_TEST_FONT").unwrap_or_default();
        let start = std::time::Instant::now();
        match font(&configured) {
            Ok(font) => {
                eprintln!("renderer font ready in {:?}", start.elapsed());
                Some(font)
            }
            Err(error) => {
                eprintln!("render samples skipped: {error}");
                None
            }
        }
    }
    #[test]
    fn task_glyph_cache_reuses_pages_and_preserves_pixels_when_evicted() {
        let Some(font) = available_font() else { return };
        assert_ne!(font.glyph_id('中'), font.glyph_id('\u{10ffff}'));
        assert!(
            rasterize(&font, '中', 22.)
                .bitmap
                .iter()
                .any(|alpha| *alpha > 0)
        );
        assert!(rasterize(&font, ' ', 22.).bitmap.is_empty());
        let cached = TaskGlyphCache::default();
        let uncached = Rc::new(RefCell::new(GlyphCache {
            max_entries: 0,
            ..GlyphCache::default()
        }));
        let bounded = Rc::new(RefCell::new(GlyphCache {
            max_entries: 3,
            max_bytes: 1024,
            ..GlyphCache::default()
        }));
        let render = |glyphs: TaskGlyphCache| {
            let mut canvas = Canvas::with_glyphs(400, 400, font.clone(), glyphs).unwrap();
            canvas.rect(0, 0, 400, 160, [221, 232, 248]);
            let text = "菜单 ABBA 123 中文菜品\n重复菜品 AABB";
            let end = canvas.text(text, 17, 19, 22., [25, 40, 55], 180);
            assert_eq!(end - 19, canvas.text_height(text, 22., 180));
            // A character at another, fractional size must not reuse its old bitmap.
            canvas.text(text, 17, end + 5, 26.5, [90, 40, 150], 350);
            canvas.text("菜单 ABBA 123", 17, 350, 22., [255, 255, 255], 350);
            canvas.pixels
        };
        let expected = render(uncached.clone());
        assert_eq!(render(cached.clone()), expected);
        let first_rasterizations = cached.borrow().rasterizations;
        assert!(first_rasterizations < uncached.borrow().rasterizations);
        assert_eq!(render(cached.clone()), expected);
        assert_eq!(cached.borrow().rasterizations, first_rasterizations);
        assert_eq!(render(bounded.clone()), expected);
        let bounded = bounded.borrow();
        assert!(bounded.entries.len() <= 3);
        assert!(bounded.bytes <= 1024);
        assert!(bounded.rasterizations > first_rasterizations);
    }
    #[test]
    fn native_render_visual_samples_and_large_menu_pagination() {
        let Some(font) = available_font() else { return };
        let output = PathBuf::from("target/native-qa");
        std::fs::create_dir_all(&output).unwrap();
        let examples = [
            (
                "calendar",
                NativeView::Calendar {
                    year: 2026,
                    month: 8,
                    subject_id: 123456789,
                    name: "用于验证自动换行的六十四字中文昵称".repeat(4),
                    days: vec![(1, 1), (8, 13), (22, 256), (31, 9)],
                },
            ),
            (
                "wordle",
                NativeView::Wordle {
                    length: 8,
                    attempts: 9,
                    guesses: vec!["aardvark".into(), "absolute".into()],
                    marks: vec![vec![2, 1, 0, 0, 1, 0, 0, 2], vec![2, 0, 1, 0, 0, 0, 1, 0]],
                    hint: Some("a**l***e".into()),
                },
            ),
            (
                "menu",
                NativeView::Menu {
                    kind: "drink".into(),
                    items: (1..=200)
                        .map(|i| format!("第{i}道 — 红豆抹茶黑糖珍珠鲜奶冰沙与温热饮品"))
                        .collect(),
                },
            ),
        ];
        for (name, view) in examples {
            let pages = native_png(view, font.clone()).unwrap();
            if name == "menu" {
                assert!(pages.len() > 1);
            }
            for (index, bytes) in pages.iter().enumerate() {
                let decoded = image::load_from_memory(bytes).unwrap();
                assert_eq!(decoded.width(), 900);
                assert!(decoded.height() <= 4000);
                std::fs::write(output.join(format!("{name}-{}.png", index + 1)), bytes).unwrap();
            }
        }
        // All 2,000 allowed entries must be included: no take(160), clipping,
        // or media-page count above the bounded 64-entry transient cache.
        let long = NativeView::Menu {
            kind: "eat".into(),
            items: (0..2000)
                .map(|i| format!("{i:04} {}", "菜".repeat(75)))
                .collect(),
        };
        let stress_start = std::time::Instant::now();
        let pages = native_png(long, font).unwrap();
        assert!((2..=64).contains(&pages.len()));
        assert!(pages.iter().all(|page| page.len() <= 8 * 1024 * 1024));
        eprintln!(
            "renderer 2000-item menu: {} pages, {:?}",
            pages.len(),
            stress_start.elapsed()
        );
    }
}
