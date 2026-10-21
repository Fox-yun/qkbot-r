//! Small CPU renderer: no browser process, HTML engine, or permanently loaded glyph atlas.
mod help;
pub use help::{HelpCard, HelpDocument, help};

use super::native::NativeView;
use crate::{host::Host, types::OutgoingMessage};
use ab_glyph::{Font as _, FontArc, FontVec, point};
use anyhow::{Context, Result, ensure};
use chrono::Datelike;
use image::ImageDecoder as _;
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
        let mut left = 0.;
        let mut top = y;
        let line = (size * 1.45).ceil() as u32;
        for ch in text.chars().take(10_000) {
            if ch == '\n' {
                left = 0.;
                top += line;
                continue;
            }
            if ch.is_control() {
                continue;
            }
            let glyph = self.glyphs.borrow_mut().get(&self.font, ch, size);
            let metrics = glyph.metrics;
            if left + metrics.advance_width > width as f32 {
                left = 0.;
                top += line;
            }
            if top + line > self.height {
                break;
            }
            let base_x = x as i32 + left as i32 + metrics.xmin;
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
pub enum ChatContent {
    Text(String),
    Image(Vec<u8>),
}
#[derive(Clone)]
pub struct ChatBubble {
    pub name: String,
    pub avatar: Option<Vec<u8>>,
    pub content: Vec<ChatContent>,
}

const CHAT_WIDTH: u32 = 900;
const CHAT_TEXT_WIDTH: u32 = 650;
const CHAT_FONT_SIZE: f32 = 30.;
const CHAT_LINE_HEIGHT: u32 = 44;
const CHAT_BACKGROUND: [u8; 3] = [242, 243, 245];

struct ChatText {
    lines: Vec<String>,
    width: u32,
}
fn chat_text(font: &Font, text: &str, size: f32, max_width: u32) -> ChatText {
    let mut lines = Vec::new();
    let mut widest = 0_f32;
    for paragraph in text.split('\n') {
        let chars: Vec<char> = paragraph.chars().filter(|ch| !ch.is_control()).collect();
        if chars.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut start = 0;
        while start < chars.len() {
            let mut end = start;
            let mut width = 0.;
            while end < chars.len() {
                let next = advance(font, chars[end], size);
                if end > start && width + next > max_width as f32 {
                    break;
                }
                width += next;
                end += 1;
            }
            if end < chars.len() {
                // Keep ordinary Latin words together when they fit on a line.
                // An overlong URL/word still wraps at a glyph boundary.
                let word = |ch: char| ch.is_ascii_alphanumeric() || "_-/.:?=&%#@".contains(ch);
                if word(chars[end - 1]) && word(chars[end]) {
                    let mut word_start = end;
                    while word_start > start && word(chars[word_start - 1]) {
                        word_start -= 1;
                    }
                    if word_start > start {
                        end = word_start;
                    }
                }
                // Avoid a closing punctuation mark at the next line's start,
                // or an opening mark stranded at this line's end.
                while end > start + 1
                    && ("，。！？；：、）》】」』”’,.!?;:)]}".contains(chars[end])
                        || "（《【「『“‘([{".contains(chars[end - 1]))
                {
                    end -= 1;
                }
            }
            let line: String = chars[start..end].iter().collect();
            let line = line.trim_end().to_owned();
            widest = widest.max(line.chars().map(|ch| advance(font, ch, size)).sum());
            lines.push(line);
            start = end;
            while start < chars.len() && chars[start].is_whitespace() {
                start += 1;
            }
        }
    }
    ChatText {
        lines,
        width: widest.ceil() as u32,
    }
}

fn chat_name(font: &Font, name: &str) -> String {
    let mut result = String::new();
    let mut width = 0.;
    let ellipsis = advance(font, '…', 22.);
    for ch in name.chars().filter(|ch| !ch.is_control()).take(256) {
        let next = advance(font, ch, 22.);
        if width + next + ellipsis > 670. {
            result.push('…');
            break;
        }
        result.push(ch);
        width += next;
    }
    result
}

fn decode_chat_image(bytes: &[u8]) -> Result<image::DynamicImage> {
    ensure!(bytes.len() <= 4 * 1024 * 1024, "聊天图片资源过大");
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    let orientation = decoder.orientation()?;
    let mut decoded = image::DynamicImage::from_decoder(decoder)?;
    decoded.apply_orientation(orientation);
    Ok(decoded)
}

enum ChatPart {
    Text(ChatText),
    Image(image::RgbaImage),
}
struct ChatLayout {
    name: String,
    avatar: Option<image::RgbaImage>,
    parts: Vec<ChatPart>,
    y: u32,
    width: u32,
    height: u32,
    padding: u32,
}
fn chat_layout(bubbles: Vec<ChatBubble>, font: &Font) -> Result<(Vec<ChatLayout>, u32)> {
    ensure!(
        !bubbles.is_empty() && bubbles.len() <= 24,
        "聊天图片最多24条消息"
    );
    let mut image_count = 0;
    let mut y = 28;
    let mut layouts = Vec::new();
    for bubble in bubbles {
        let mut parts = Vec::new();
        let mut content_width = 0;
        let mut content_height = 0;
        let mut characters = 0;
        let has_text = bubble
            .content
            .iter()
            .any(|part| matches!(part, ChatContent::Text(text) if !text.trim().is_empty()));
        let padding = if has_text { 18 } else { 0 };
        for part in bubble.content {
            let (part, width, height) = match part {
                ChatContent::Text(text) => {
                    characters += text.chars().count();
                    ensure!(characters <= 10_000, "单条聊天文字过长");
                    if text.is_empty() {
                        continue;
                    }
                    let text = chat_text(font, &text, CHAT_FONT_SIZE, CHAT_TEXT_WIDTH);
                    let width = text.width;
                    let height = text.lines.len() as u32 * CHAT_LINE_HEIGHT;
                    (ChatPart::Text(text), width, height)
                }
                ChatContent::Image(bytes) => {
                    image_count += 1;
                    ensure!(image_count <= 8, "聊天图片一次最多8张图片");
                    let decoded = decode_chat_image(&bytes)?;
                    let decoded = decoded
                        .thumbnail(decoded.width().min(580), decoded.height().min(440))
                        .to_rgba8();
                    let width = decoded.width();
                    let height = decoded.height();
                    (ChatPart::Image(decoded), width, height)
                }
            };
            if !parts.is_empty() {
                content_height += 12;
            }
            content_width = content_width.max(width);
            content_height += height;
            parts.push(part);
        }
        ensure!(!parts.is_empty(), "聊天消息没有可绘制内容");
        let width = (content_width + padding * 2).max(60);
        let height = (content_height + if has_text { 24 } else { 0 }).max(44);
        let end = (y + 34 + height).max(y + 72);
        ensure!(end + 28 <= 8192, "聊天图片过长，无法完整绘制");
        let avatar = bubble.avatar.and_then(|bytes| {
            decode_chat_image(&bytes).ok().map(|decoded| {
                decoded
                    .resize_to_fill(72, 72, image::imageops::FilterType::Triangle)
                    .to_rgba8()
            })
        });
        layouts.push(ChatLayout {
            name: chat_name(font, &bubble.name),
            avatar,
            parts,
            y,
            width,
            height,
            padding,
        });
        y = end + 28;
    }
    Ok((layouts, y))
}

fn rounded_coverage(x: u32, y: u32, width: u32, height: u32, radius: u32) -> f32 {
    let radius = radius.min(width / 2).min(height / 2) as f32;
    if radius == 0. {
        return 1.;
    }
    let px = x as f32 + 0.5;
    let py = y as f32 + 0.5;
    let dx = (radius - px).max(px - (width as f32 - radius)).max(0.);
    let dy = (radius - py).max(py - (height as f32 - radius)).max(0.);
    (radius + 0.5 - dx.hypot(dy)).clamp(0., 1.)
}
fn chat_pixel(canvas: &mut Canvas, x: u32, y: u32, color: [u8; 3], alpha: f32) {
    if x >= canvas.width || y >= canvas.height {
        return;
    }
    let pixel = ((y * canvas.width + x) * 3) as usize;
    for (channel, value) in color.into_iter().enumerate() {
        let background = canvas.pixels[pixel + channel] as f32;
        canvas.pixels[pixel + channel] =
            (background * (1. - alpha) + value as f32 * alpha).round() as u8;
    }
}
fn chat_round_rect(canvas: &mut Canvas, rect: [u32; 4], radius: u32, color: [u8; 3]) {
    let [x, y, width, height] = rect;
    for yy in 0..height {
        for xx in 0..width {
            chat_pixel(
                canvas,
                x + xx,
                y + yy,
                color,
                rounded_coverage(xx, yy, width, height, radius),
            );
        }
    }
}
fn chat_picture(canvas: &mut Canvas, picture: &image::RgbaImage, x: u32, y: u32, radius: u32) {
    for (xx, yy, pixel) in picture.enumerate_pixels() {
        let alpha = pixel[3] as f32 / 255.
            * rounded_coverage(xx, yy, picture.width(), picture.height(), radius);
        chat_pixel(
            canvas,
            x + xx,
            y + yy,
            [pixel[0], pixel[1], pixel[2]],
            alpha,
        );
    }
}
fn chat_avatar(canvas: &mut Canvas, avatar: Option<&image::RgbaImage>, x: u32, y: u32) {
    chat_round_rect(canvas, [x, y, 72, 72], 36, [214, 222, 233]);
    if let Some(avatar) = avatar {
        chat_picture(canvas, avatar, x, y, 36);
        return;
    }
    // Apply the same circular mask to the entire fallback silhouette, including
    // its shoulders at the bottom of the avatar.
    for yy in 0..72 {
        for xx in 0..72 {
            let head = if (24..48).contains(&xx) && (13..37).contains(&yy) {
                rounded_coverage(xx - 24, yy - 13, 24, 24, 12)
            } else {
                0.
            };
            let body = if (12..60).contains(&xx) && (40..72).contains(&yy) {
                rounded_coverage(xx - 12, yy - 40, 48, 32, 16)
            } else {
                0.
            };
            let alpha = head.max(body) * rounded_coverage(xx, yy, 72, 72, 36);
            chat_pixel(canvas, x + xx, y + yy, [249, 250, 252], alpha);
        }
    }
}
fn chat_png(bubbles: Vec<ChatBubble>, font: Arc<Font>) -> Result<Vec<u8>> {
    let (layouts, height) = chat_layout(bubbles, &font)?;
    let mut canvas = Canvas::new(CHAT_WIDTH, height, font)?;
    canvas.rect(0, 0, CHAT_WIDTH, height, CHAT_BACKGROUND);
    for layout in layouts {
        chat_avatar(&mut canvas, layout.avatar.as_ref(), 28, layout.y);
        canvas.text(&layout.name, 126, layout.y, 22., [124, 129, 138], 690);
        let top = layout.y + 34;
        chat_round_rect(
            &mut canvas,
            [126, top + 1, layout.width, layout.height],
            14,
            [230, 232, 235],
        );
        chat_round_rect(
            &mut canvas,
            [126, top, layout.width, layout.height],
            14,
            [255; 3],
        );
        if layout.padding > 0 {
            for row in 0_u32..14 {
                let width = 7 - row.abs_diff(6).min(7);
                canvas.rect(126 - width, top + 14 + row, width + 1, 1, [255; 3]);
            }
        }
        let mut y = top + if layout.padding > 0 { 12 } else { 0 };
        for part in layout.parts {
            match part {
                ChatPart::Text(text) => {
                    for line in text.lines {
                        canvas.text(
                            &line,
                            126 + layout.padding,
                            y,
                            CHAT_FONT_SIZE,
                            [32, 34, 38],
                            CHAT_TEXT_WIDTH + 1,
                        );
                        y += CHAT_LINE_HEIGHT;
                    }
                }
                ChatPart::Image(picture) => {
                    chat_picture(&mut canvas, &picture, 126 + layout.padding, y, 12);
                    y += picture.height();
                }
            }
            y += 12;
        }
    }
    canvas.png(height)
}

pub async fn chat(host: &Host, bubbles: Vec<ChatBubble>) -> Result<OutgoingMessage> {
    let path = host.config.plugins.native.image_font_path.clone();
    let permit = host
        .media
        .rendering
        .clone()
        .try_acquire_owned()
        .context("绘图繁忙")?;
    let bytes = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        chat_png(bubbles, font(&path)?)
    })
    .await??;
    host.media.png(host, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chat_message(name: &str, text: &str) -> ChatBubble {
        ChatBubble {
            name: name.into(),
            avatar: None,
            content: vec![ChatContent::Text(text.into())],
        }
    }
    fn rgba_png(picture: image::RgbaImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(picture)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
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
    fn text_height_matches_render_at_fractional_wrap_boundaries() {
        let Some(font) = available_font() else { return };
        let mut canvas = Canvas::new(1100, 4000, font.clone()).unwrap();
        // Quantize an accumulated advance to an integer width, then test both
        // sides of that boundary at different absolute drawing coordinates.
        for ch in ['W', 'i', '中', '0'] {
            for size in [17., 21., 24.5] {
                let text = ch.to_string().repeat(30);
                let boundary = (advance(&font, ch, size) * 10.).round() as u32;
                for width in [boundary.saturating_sub(1), boundary, boundary + 1] {
                    let expected = canvas.text_height(&text, size, width);
                    for x in [0, 50, 311] {
                        let bottom = canvas.text(&text, x, 17, size, [0; 3], width);
                        assert_eq!(bottom - 17, expected, "{ch} / {size} / {width} / {x}");
                    }
                }
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

    #[test]
    fn chat_layout_wraps_complete_text_and_rejects_oversized_last_message() {
        let Some(font) = available_font() else { return };
        for text in ["中".repeat(240), "W".repeat(320), "甲\n\n乙\n丙".into()] {
            let layout = chat_text(&font, &text, CHAT_FONT_SIZE, CHAT_TEXT_WIDTH);
            assert!(layout.width <= CHAT_TEXT_WIDTH);
            assert_eq!(layout.lines.join(""), text.replace('\n', ""));
            let (rows, height) =
                chat_layout(vec![chat_message("长文字测试", &text)], &font).unwrap();
            assert_eq!(
                rows[0].height,
                layout.lines.len() as u32 * CHAT_LINE_HEIGHT + 24
            );
            assert_eq!(height, 28 + 34 + rows[0].height + 28);
        }
        let words = chat_text(&font, "alpha bravo charlie delta", CHAT_FONT_SIZE, 155);
        assert_eq!(words.lines, ["alpha", "bravo", "charlie", "delta"]);
        let paragraph = chat_text(
            &font,
            &"汉字，句号。括号（内容）".repeat(12),
            CHAT_FONT_SIZE,
            CHAT_TEXT_WIDTH,
        );
        assert!(
            paragraph
                .lines
                .iter()
                .skip(1)
                .all(|line| !line.starts_with(['，', '。', '）']))
        );
        assert!(chat_layout(vec![chat_message("超长", &"字".repeat(10_001))], &font).is_err());
        assert!(
            chat_layout(
                vec![
                    chat_message("第一条", "可以正常绘制"),
                    chat_message("最后一条", &"长行\n".repeat(190))
                ],
                &font
            )
            .is_err()
        );
        assert!(chat_layout(vec![chat_message("多人", "消息"); 25], &font).is_err());
        assert!(
            chat_layout(
                vec![ChatBubble {
                    name: "太多图片".into(),
                    avatar: None,
                    content: (0..9)
                        .map(
                            |_| ChatContent::Image(rgba_png(image::RgbaImage::from_pixel(
                                2,
                                2,
                                image::Rgba([255; 4])
                            )))
                        )
                        .collect(),
                }],
                &font
            )
            .is_err()
        );
        let wide = rgba_png(image::RgbaImage::from_pixel(8193, 1, image::Rgba([255; 4])));
        assert!(decode_chat_image(&wide).is_err());
    }

    #[test]
    fn chat_images_preserve_alpha_and_small_dimensions_and_bad_avatars_fall_back() {
        let Some(font) = available_font() else { return };
        let picture = image::RgbaImage::from_pixel(4, 3, image::Rgba([200, 50, 20, 128]));
        let mut canvas = Canvas::new(20, 20, font.clone()).unwrap();
        canvas.rect(0, 0, 20, 20, [100, 100, 100]);
        chat_picture(&mut canvas, &picture, 2, 2, 0);
        let pixel = ((2 * 20 + 2) * 3) as usize;
        assert_eq!(&canvas.pixels[pixel..pixel + 3], &[150, 75, 60]);
        let (rows, _) = chat_layout(
            vec![ChatBubble {
                name: "无头像".into(),
                avatar: Some(vec![0, 1, 2]),
                content: vec![ChatContent::Image(rgba_png(picture))],
            }],
            &font,
        )
        .unwrap();
        assert!(rows[0].avatar.is_none());
        let ChatPart::Image(picture) = &rows[0].parts[0] else {
            panic!("expected image")
        };
        assert_eq!(picture.dimensions(), (4, 3));
        assert!(
            chat_layout(
                vec![ChatBubble {
                    name: "坏图片".into(),
                    avatar: None,
                    content: vec![ChatContent::Image(vec![0, 1, 2])],
                }],
                &font
            )
            .is_err()
        );
    }

    #[test]
    fn chat_camera_photo_orientation_is_applied_before_layout() {
        use image::ImageEncoder as _;
        let exif = vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0,
            0,
        ];
        let source = image::RgbImage::from_fn(32, 16, |x, _| {
            image::Rgb(if x < 16 { [240, 20, 20] } else { [20, 20, 240] })
        });
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .encode(source.as_raw(), 32, 16, image::ExtendedColorType::Rgb8)
            .unwrap();
        let decoded = decode_chat_image(&bytes).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (16, 32));
        assert!(decoded.get_pixel(8, 4)[0] > 180);
        assert!(decoded.get_pixel(8, 28)[2] > 180);
    }

    #[test]
    fn chat_real_and_fallback_avatars_are_clipped_to_a_72_pixel_circle() {
        let Some(font) = available_font() else { return };
        let picture = image::RgbaImage::from_pixel(72, 72, image::Rgba([220, 40, 80, 255]));
        for avatar in [Some(&picture), None] {
            let mut canvas = Canvas::new(80, 80, font.clone()).unwrap();
            canvas.rect(0, 0, 80, 80, CHAT_BACKGROUND);
            chat_avatar(&mut canvas, avatar, 4, 4);
            for y in 0..80 {
                for x in 0..80 {
                    let inside = (4..76).contains(&x) && (4..76).contains(&y);
                    if !inside || rounded_coverage(x - 4, y - 4, 72, 72, 36) == 0. {
                        let at = ((y * 80 + x) * 3) as usize;
                        assert_eq!(
                            &canvas.pixels[at..at + 3],
                            &CHAT_BACKGROUND,
                            "outside circle at {x},{y}"
                        );
                    }
                }
            }
            let center = ((40 * 80 + 40) * 3) as usize;
            assert_ne!(&canvas.pixels[center..center + 3], &CHAT_BACKGROUND);
            assert_eq!(rounded_coverage(0, 0, 72, 72, 36), 0.);
            assert!(rounded_coverage(35, 0, 72, 72, 36) > 0.9);
        }
    }

    #[test]
    fn chat_render_visual_samples_cover_multiplayer_wrapping_and_mixed_images() {
        let Some(font) = available_font() else { return };
        let output = PathBuf::from("target/chat-qa");
        std::fs::create_dir_all(&output).unwrap();
        let avatar = |letter: &str, color: [u8; 3]| {
            let mut canvas = Canvas::new(72, 72, font.clone()).unwrap();
            canvas.rect(0, 0, 72, 72, color);
            canvas.text(letter, 19, 17, 34., [255; 3], 50);
            canvas.png(72).unwrap()
        };
        let avatars = [
            avatar("舟", [97, 151, 176]),
            avatar("橙", [211, 156, 100]),
            avatar("川", [119, 155, 128]),
        ];
        let mut multiplayer = vec![
            chat_message("小舟", "周末去江边走走吗？"),
            chat_message("阿橙", "好呀，下午应该不会太晒。"),
            chat_message("晴川", "我也来！带相机。"),
            chat_message("小舟", "那就在地铁站出口见，沿着江边慢慢走。"),
            chat_message("阿橙", "好，我带点喝的。"),
        ];
        for (index, bubble) in multiplayer.iter_mut().enumerate() {
            bubble.avatar = Some(avatars[index % 3].clone());
        }
        let mut long = vec![
            chat_message(
                &"晴川的很长很长的群昵称".repeat(5),
                "这段文字用来检查聊天气泡的自动换行：中文标点应当和前面的文字连在一起，括号（例如这里的说明）也应该自然排版。内容变长时，气泡会向下延伸，下一条消息不会挡住结尾。\n\n这里保留一个空行，然后继续下一段。",
            ),
            chat_message(
                "小舟",
                "The updated layout keeps ordinary English words together and wraps long addresses when necessary.\nhttps://example.invalid/this-is-a-very-long-path-that-must-wrap-without-losing-any-characters",
            ),
            chat_message("阿橙", "收到，最后一条也能完整看到。"),
        ];
        for (index, bubble) in long.iter_mut().enumerate() {
            bubble.avatar = Some(avatars[(index + 2) % 3].clone());
        }
        let mut card = Canvas::new(520, 280, font.clone()).unwrap();
        card.rect(0, 0, 520, 280, [227, 239, 247]);
        chat_round_rect(&mut card, [25, 25, 470, 230], 16, [255; 3]);
        card.text("今天的进度", 49, 45, 30., [38, 69, 87], 400);
        card.text("页面排版已经完成", 49, 101, 23., [94, 113, 123], 400);
        for (i, width) in [380, 294, 340].into_iter().enumerate() {
            chat_round_rect(
                &mut card,
                [49, 156 + i as u32 * 26, width, 12],
                6,
                [131, 181, 191],
            );
        }
        let card = card.png(280).unwrap();
        let transparent = rgba_png(image::RgbaImage::from_fn(180, 100, |x, y| {
            let inside = (x as i32 - 90).pow(2) + (y as i32 - 50).pow(2) < 38 * 38;
            image::Rgba(if inside {
                [237, 172, 77, 220]
            } else {
                [0, 0, 0, 0]
            })
        }));
        let mixed = vec![
            ChatBubble {
                name: "小舟".into(),
                avatar: Some(avatars[0].clone()),
                content: vec![
                    ChatContent::Text("这张是刚整理好的进度图。".into()),
                    ChatContent::Image(card.clone()),
                    ChatContent::Text("透明底的小图也保留了原来的尺寸：".into()),
                    ChatContent::Image(transparent),
                    ChatContent::Text("图片后面的说明还在这里，顺序没有变。".into()),
                ],
            },
            chat_message("阿橙", "看到了，排版很清楚。"),
            ChatBubble {
                name: "晴川".into(),
                avatar: Some(avatars[2].clone()),
                content: vec![ChatContent::Image(card)],
            },
        ];
        for (name, bubbles) in [
            ("multiplayer", multiplayer),
            ("wrapping", long),
            ("mixed-images", mixed),
        ] {
            let bytes = chat_png(bubbles, font.clone()).unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!(decoded.width(), CHAT_WIDTH);
            assert!(decoded.height() <= 8192);
            assert!(bytes.len() <= 8 * 1024 * 1024);
            std::fs::write(output.join(format!("{name}.png")), bytes).unwrap();
        }
    }
}
