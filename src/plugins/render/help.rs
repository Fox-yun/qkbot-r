//! Structured help cards. Layout and drawing share the same measured lines.
use super::{Canvas, Font, TaskGlyphCache, advance, font};
use crate::{host::Host, types::OutgoingMessage};
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct HelpDocument {
    pub title: String,
    pub subtitle: String,
    pub cards: Vec<HelpCard>,
    pub footer: Vec<String>,
    pub detail: bool,
}

#[derive(Clone, Debug)]
pub struct HelpCard {
    pub number: Option<usize>,
    pub title: String,
    pub description: String,
    pub commands: Vec<String>,
    pub badges: Vec<String>,
}

const WIDTH: u32 = 1080;
const MAX_HEIGHT: u32 = 2200;
const MAX_PAGES: usize = 16;
const MARGIN: u32 = 40;
const GAP: u32 = 20;
const PADDING: u32 = 24;
const BACKGROUND: [u8; 3] = [243, 246, 249];
const NAVY: [u8; 3] = [30, 50, 77];
const TEXT: [u8; 3] = [66, 81, 102];
const MUTED: [u8; 3] = [106, 122, 139];
const TEAL: [u8; 3] = [20, 131, 124];
const TINT: [u8; 3] = [234, 246, 243];
const COMMAND: [u8; 3] = [241, 245, 249];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Title,
    Description,
    Command,
    Badge,
    Space,
}
#[derive(Clone, Debug)]
struct Line {
    text: String,
    kind: Kind,
    size: f32,
    height: u32,
    inset: u32,
}
#[derive(Debug)]
struct Card {
    #[cfg(test)]
    source: usize,
    number: Option<usize>,
    width: u32,
    lines: Vec<Line>,
    note: bool,
}
#[derive(Debug)]
struct Fragment {
    #[cfg(test)]
    source: usize,
    number: Option<usize>,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    lines: Vec<Line>,
    continued: bool,
    note: bool,
}
#[derive(Debug)]
struct Page {
    cards: Vec<Fragment>,
    end: u32,
}
#[derive(Debug)]
struct Layout {
    title: Vec<Line>,
    subtitle: Vec<Line>,
    pages: Vec<Page>,
    top: u32,
}

fn line_height(size: f32) -> u32 {
    (size * 1.45).ceil() as u32
}

// Do not split by byte count: full-width CJK and narrow Latin text differ.
// Keeping these exact lines for drawing makes pagination independent of the
// Canvas helper's 10,000-character guard and avoids silently clipped commands.
fn wrap(font: &Font, text: &str, size: f32, width: u32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used = 0.;
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut current));
            used = 0.;
        } else if ch.is_control() {
            continue;
        } else {
            let next = advance(font, ch, size);
            if used + next > width as f32 && !current.is_empty() {
                // Avoid an isolated full stop or closing bracket on the next
                // line, without omitting punctuation or exceeding the width.
                let carry = if current.chars().count() > 1
                    && ("，。！？、；：）》】」』,.!?;:)".contains(ch)
                        || current.ends_with(['（', '《', '【', '「', '『', '(']))
                {
                    current.pop()
                } else {
                    None
                };
                lines.push(std::mem::take(&mut current));
                used = 0.;
                if let Some(previous) = carry {
                    current.push(previous);
                    used = advance(font, previous, size);
                }
            }
            current.push(ch);
            used += next;
        }
    }
    if !current.is_empty() || lines.is_empty() || text.ends_with('\n') {
        lines.push(current);
    }
    lines
}

fn add_text(
    lines: &mut Vec<Line>,
    font: &Font,
    text: &str,
    kind: Kind,
    size: f32,
    width: u32,
    inset: u32,
) {
    if text.is_empty() {
        return;
    }
    lines.extend(
        wrap(font, text, size, width - inset)
            .into_iter()
            .map(|text| Line {
                text,
                kind,
                size,
                height: line_height(size),
                inset,
            }),
    );
}
fn space(lines: &mut Vec<Line>, height: u32) {
    if !lines.is_empty() {
        lines.push(Line {
            text: String::new(),
            kind: Kind::Space,
            size: 0.,
            height,
            inset: 0,
        });
    }
}
fn card(font: &Font, input: &HelpCard, _source: usize, width: u32) -> Card {
    let inner = width - PADDING * 2;
    let mut lines = Vec::new();
    let inset = if input.number.is_some() { 58 } else { 0 };
    add_text(
        &mut lines,
        font,
        &input.title,
        Kind::Title,
        28.,
        inner,
        inset,
    );
    if !input.badges.is_empty() {
        space(&mut lines, 10);
        add_text(
            &mut lines,
            font,
            &input.badges.join(" · "),
            Kind::Badge,
            18.,
            inner - 20,
            10,
        );
    }
    if !input.description.is_empty() {
        space(&mut lines, 14);
        add_text(
            &mut lines,
            font,
            &input.description,
            Kind::Description,
            22.,
            inner,
            0,
        );
    }
    if !input.commands.is_empty() {
        space(&mut lines, 16);
        add_text(&mut lines, font, "使用方式", Kind::Badge, 18., inner, 0);
        space(&mut lines, 7);
        for (index, command) in input.commands.iter().enumerate() {
            if index > 0 {
                space(&mut lines, 8);
            }
            add_text(
                &mut lines,
                font,
                command,
                Kind::Command,
                21.,
                inner - 12,
                12,
            );
        }
    }
    Card {
        #[cfg(test)]
        source: _source,
        number: input.number,
        width,
        lines,
        note: false,
    }
}

fn validate(document: &HelpDocument) -> Result<()> {
    ensure!(!document.title.trim().is_empty(), "帮助标题不能为空");
    ensure!(document.title.chars().count() <= 120, "帮助标题过长");
    ensure!(document.subtitle.chars().count() <= 500, "帮助副标题过长");
    ensure!(document.cards.len() <= 96, "帮助条目过多");
    let mut bytes = document.title.len() + document.subtitle.len();
    for item in &document.cards {
        ensure!(!item.title.trim().is_empty(), "帮助条目标题不能为空");
        ensure!(
            item.commands.len() <= 128 && item.badges.len() <= 16,
            "帮助条目内容过多"
        );
        ensure!(
            item.number.is_none_or(|number| number <= 999),
            "帮助编号过大"
        );
        bytes += item.title.len() + item.description.len();
        bytes += item
            .commands
            .iter()
            .chain(&item.badges)
            .map(String::len)
            .sum::<usize>();
    }
    ensure!(document.footer.len() <= 32, "帮助说明条目过多");
    bytes += document.footer.iter().map(String::len).sum::<usize>();
    ensure!(bytes <= 128 * 1024, "帮助内容过长");
    Ok(())
}

fn layout(document: &HelpDocument, font: &Font) -> Result<Layout> {
    validate(document)?;
    let inner = WIDTH - MARGIN * 2;
    let mut title = Vec::new();
    add_text(
        &mut title,
        font,
        &document.title,
        Kind::Title,
        38.,
        inner - 20,
        0,
    );
    let mut subtitle = Vec::new();
    add_text(
        &mut subtitle,
        font,
        &document.subtitle,
        Kind::Description,
        21.,
        inner,
        0,
    );
    let top = 38
        + title.iter().map(|line| line.height).sum::<u32>()
        + 12
        + subtitle.iter().map(|line| line.height).sum::<u32>()
        + 28;
    ensure!(top < MAX_HEIGHT / 2, "帮助页头过长");
    let columns = if document.detail { 1 } else { 2 };
    let width = if columns == 1 {
        inner
    } else {
        (inner - GAP) / 2
    };
    let mut rows: Vec<Vec<Card>> = Vec::new();
    for (index, input) in document.cards.iter().enumerate() {
        if index % columns == 0 {
            rows.push(Vec::new());
        }
        rows.last_mut()
            .unwrap()
            .push(card(font, input, index, width));
    }
    if !document.footer.is_empty() || document.cards.is_empty() {
        let mut lines = Vec::new();
        let notes = if document.footer.is_empty() {
            vec!["当前没有可用的帮助条目。".to_owned()]
        } else {
            document.footer.clone()
        };
        for note in notes {
            space(&mut lines, 6);
            add_text(
                &mut lines,
                font,
                &note,
                Kind::Description,
                20.,
                inner - PADDING * 2,
                0,
            );
        }
        rows.push(vec![Card {
            #[cfg(test)]
            source: document.cards.len(),
            number: None,
            width: inner,
            lines,
            note: true,
        }]);
    }

    let bottom = MAX_HEIGHT - 64;
    let mut pages = vec![Page {
        cards: Vec::new(),
        end: top,
    }];
    for row in rows {
        let full_height = row
            .iter()
            .map(|c| PADDING * 2 + c.lines.iter().map(|line| line.height).sum::<u32>())
            .max()
            .unwrap_or(0);
        let page = pages.last().unwrap();
        // Keep ordinary rows together. A row too tall for a fresh page is
        // split by measured lines, with both columns staying in their slots.
        if page.end > top && page.end + full_height > bottom {
            ensure!(pages.len() < MAX_PAGES, "帮助页数过多");
            pages.push(Page {
                cards: Vec::new(),
                end: top,
            });
        }
        let mut offsets = vec![0; row.len()];
        loop {
            let page = pages.last_mut().unwrap();
            let y = page.end;
            let mut row_height = 0;
            for (column, c) in row.iter().enumerate() {
                let start = offsets[column];
                if start == c.lines.len() {
                    continue;
                }
                let continued = start > 0;
                let extra = if continued { 32 } else { 0 };
                let mut height = PADDING * 2 + extra;
                let mut end = start;
                while end < c.lines.len() && y + height + c.lines[end].height <= bottom {
                    height += c.lines[end].height;
                    end += 1;
                }
                ensure!(end > start, "帮助单行无法排版");
                page.cards.push(Fragment {
                    #[cfg(test)]
                    source: c.source,
                    number: if continued { None } else { c.number },
                    x: MARGIN + column as u32 * (width + GAP),
                    y,
                    width: c.width,
                    height,
                    lines: c.lines[start..end].to_vec(),
                    continued,
                    note: c.note,
                });
                offsets[column] = end;
                row_height = row_height.max(height);
            }
            // Matching card bottoms give the two-column menu a calm grid.
            for fragment in page.cards.iter_mut().rev().take_while(|f| f.y == y) {
                fragment.height = row_height;
            }
            page.end = y + row_height + GAP;
            if row
                .iter()
                .zip(&offsets)
                .all(|(c, offset)| *offset == c.lines.len())
            {
                break;
            }
            ensure!(pages.len() < MAX_PAGES, "帮助页数过多");
            pages.push(Page {
                cards: Vec::new(),
                end: top,
            });
        }
    }
    Ok(Layout {
        title,
        subtitle,
        pages,
        top,
    })
}

fn rounded(
    canvas: &mut Canvas,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    radius: u32,
    color: [u8; 3],
) {
    let radius = radius.min(width / 2).min(height / 2);
    for row in 0..height {
        let distance = if row < radius {
            (radius - row) as f32 - 0.5
        } else if row >= height - radius {
            (row - (height - radius)) as f32 + 0.5
        } else {
            0.
        };
        let inset = if distance > 0. {
            (radius as f32 - ((radius * radius) as f32 - distance * distance).sqrt()).round() as u32
        } else {
            0
        };
        canvas.rect(x + inset, y + row, width - inset * 2, 1, color);
    }
}

fn draw_fragment(canvas: &mut Canvas, fragment: &Fragment) {
    let x = fragment.x;
    let y = fragment.y;
    rounded(
        canvas,
        x,
        y + 3,
        fragment.width,
        fragment.height,
        18,
        [226, 233, 239],
    );
    rounded(
        canvas,
        x,
        y,
        fragment.width,
        fragment.height,
        18,
        if fragment.note {
            [231, 240, 243]
        } else {
            [255, 255, 255]
        },
    );
    let mut top = y + PADDING;
    if fragment.continued {
        canvas.text(
            "接上页",
            x + PADDING,
            top,
            18.,
            TEAL,
            fragment.width - PADDING * 2,
        );
        top += 32;
    }
    if let Some(number) = fragment.number {
        rounded(canvas, x + PADDING, top, 44, 42, 12, TINT);
        let number = number.to_string();
        let size = if number.len() > 2 { 20. } else { 24. };
        let measured = number
            .chars()
            .map(|ch| advance(&canvas.font, ch, size))
            .sum::<f32>();
        canvas.text(
            &number,
            x + PADDING + ((44. - measured) / 2.).max(0.) as u32,
            top + 3,
            size,
            TEAL,
            44,
        );
    }
    for line in &fragment.lines {
        let color = match line.kind {
            Kind::Title => NAVY,
            Kind::Description => TEXT,
            Kind::Command => [43, 72, 104],
            Kind::Badge => TEAL,
            Kind::Space => {
                top += line.height;
                continue;
            }
        };
        if line.kind == Kind::Command {
            canvas.rect(
                x + PADDING,
                top,
                fragment.width - PADDING * 2,
                line.height,
                COMMAND,
            );
            canvas.rect(x + PADDING, top, 3, line.height, [139, 185, 185]);
        }
        canvas.text(
            &line.text,
            x + PADDING + line.inset,
            top,
            line.size,
            color,
            fragment.width - PADDING * 2 - line.inset,
        );
        top += line.height;
    }
    debug_assert!(top <= y + fragment.height - PADDING);
}

fn help_png(document: &HelpDocument, font: Arc<Font>) -> Result<Vec<Vec<u8>>> {
    let layout = layout(document, &font)?;
    let glyphs = TaskGlyphCache::default();
    let mut pages = Vec::new();
    let mut total_bytes = 0;
    for (index, page) in layout.pages.iter().enumerate() {
        let height = (page.end + 52).max(layout.top + 100).min(MAX_HEIGHT);
        let mut canvas = Canvas::with_glyphs(WIDTH, height, font.clone(), glyphs.clone())?;
        canvas.rect(0, 0, WIDTH, height, BACKGROUND);
        rounded(&mut canvas, MARGIN, 23, 44, 5, 2, TEAL);
        let mut top = 38;
        for line in &layout.title {
            canvas.text(&line.text, MARGIN, top, line.size, NAVY, WIDTH - MARGIN * 2);
            top += line.height;
        }
        top += 12;
        for line in &layout.subtitle {
            canvas.text(
                &line.text,
                MARGIN,
                top,
                line.size,
                MUTED,
                WIDTH - MARGIN * 2,
            );
            top += line.height;
        }
        for fragment in &page.cards {
            draw_fragment(&mut canvas, fragment);
        }
        canvas.text("小鱼 · 插件帮助", MARGIN, height - 38, 17., MUTED, 500);
        let indicator = format!("{:02} / {:02}", index + 1, layout.pages.len());
        canvas.text(
            &indicator,
            WIDTH - MARGIN - 100,
            height - 38,
            17.,
            MUTED,
            100,
        );
        let bytes = canvas.png(height)?;
        total_bytes += bytes.len();
        ensure!(total_bytes <= 16 * 1024 * 1024, "帮助图片总量过大");
        pages.push(bytes);
    }
    Ok(pages)
}

pub async fn help(host: &Host, document: HelpDocument) -> Result<OutgoingMessage> {
    validate(&document)?;
    let path = host.config.plugins.native.image_font_path.clone();
    let permit = host
        .media
        .rendering
        .clone()
        .try_acquire_owned()
        .context("绘图繁忙，请稍后重试")?;
    let pages = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        help_png(&document, font(&path)?)
    })
    .await??;
    let mut segments = Vec::new();
    for bytes in pages {
        segments.extend(host.media.png(host, bytes).await?.segments);
    }
    Ok(OutgoingMessage { segments })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_font() -> Option<Arc<Font>> {
        match font(&std::env::var("QKBOT_TEST_FONT").unwrap_or_default()) {
            Ok(font) => Some(font),
            Err(error) => {
                eprintln!("help image tests skipped: {error}");
                None
            }
        }
    }
    fn sample() -> HelpDocument {
        let config = crate::config::Config::default();
        HelpDocument {
            title: "小鱼 · 功能帮助".into(),
            subtitle: "选一个感兴趣的功能，发送「/帮助 编号」查看完整用法。".into(),
            cards: crate::catalog::features(&config)
                .into_iter()
                .filter(|feature| feature.enabled)
                .enumerate()
                .map(|(index, feature)| HelpCard {
                    number: Some(index + 1),
                    title: feature.name,
                    description: feature.description,
                    commands: feature.commands.into_iter().take(2).collect(),
                    badges: if feature.group_only {
                        vec!["群聊".into()]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
            footer: vec![
                "/帮助 序号或名称 查看详情；可再加命令序号。".into(),
                "卡片展示常用入口；详情中可查看全部命令。".into(),
            ],
            detail: false,
        }
    }

    #[test]
    fn help_layout_preserves_row_major_numbers_and_card_boundaries() {
        let Some(font) = test_font() else { return };
        let document = sample();
        let layout = layout(&document, &font).unwrap();
        let numbered: Vec<_> = layout
            .pages
            .iter()
            .flat_map(|p| &p.cards)
            .filter_map(|f| f.number.map(|number| (number, f.source, f.x, f.y)))
            .collect();
        assert_eq!(
            numbered.iter().map(|v| v.0).collect::<Vec<_>>(),
            (1..=document.cards.len()).collect::<Vec<_>>()
        );
        for pair in numbered.as_chunks::<2>().0 {
            assert_eq!(pair[0].3, pair[1].3);
            assert!(pair[0].2 < pair[1].2);
        }
        for page in &layout.pages {
            for fragment in &page.cards {
                assert!(fragment.x + fragment.width <= WIDTH - MARGIN);
                assert!(fragment.y + fragment.height <= MAX_HEIGHT - 64);
                assert!(
                    fragment.lines.iter().map(|line| line.height).sum::<u32>() + PADDING * 2
                        <= fragment.height
                );
            }
        }
    }

    #[test]
    fn help_long_commands_paginate_without_loss_or_repeated_number() {
        let Some(font) = test_font() else { return };
        let command = format!("/示例 {}", "长命令参数abc-123_".repeat(650));
        let mut document = sample();
        document.cards.truncate(1);
        document.detail = true;
        document.cards[0].commands = vec![command.clone()];
        let layout = layout(&document, &font).unwrap();
        assert!(layout.pages.len() > 1 && layout.pages.len() <= MAX_PAGES);
        let fragments: Vec<_> = layout
            .pages
            .iter()
            .flat_map(|p| &p.cards)
            .filter(|f| f.source == 0)
            .collect();
        assert_eq!(fragments.iter().filter(|f| f.number.is_some()).count(), 1);
        let output: String = fragments
            .iter()
            .flat_map(|f| &f.lines)
            .filter(|line| line.kind == Kind::Command)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(output, command);
        assert!(fragments.iter().skip(1).all(|f| f.continued));
        for fragment in fragments {
            for line in &fragment.lines {
                let width: f32 = line
                    .text
                    .chars()
                    .map(|ch| advance(&font, ch, line.size))
                    .sum();
                assert!(width <= (fragment.width - PADDING * 2 - line.inset) as f32 + 0.1);
            }
        }
    }

    #[test]
    fn help_over_limit_rejects_instead_of_truncating() {
        let Some(font) = test_font() else { return };
        let mut document = sample();
        document.cards[0].description = "中文说明".repeat(40_000);
        assert!(
            layout(&document, &font)
                .unwrap_err()
                .to_string()
                .contains("过长")
        );
        document.cards[0].description = "完整内容".repeat(8_000);
        assert!(
            layout(&document, &font)
                .unwrap_err()
                .to_string()
                .contains("页数")
        );
    }

    #[test]
    fn help_render_qa_menu_detail_and_pagination() {
        let Some(font) = test_font() else { return };
        let output = std::path::Path::new("target/help-qa");
        std::fs::create_dir_all(output).unwrap();
        let menu = sample();
        let mut detail = menu.clone();
        detail.title = "Wordle · 使用指南".into();
        detail.subtitle = "英文猜词小游戏 · 当前群共享一局".into();
        detail.detail = true;
        let feature = crate::catalog::features(&crate::config::Config::default())
            .into_iter()
            .find(|feature| feature.id == "wordle")
            .unwrap();
        let number = menu
            .cards
            .iter()
            .position(|card| card.title == feature.name)
            .unwrap()
            + 1;
        detail.cards = vec![HelpCard {
            number: Some(number),
            title: feature.name,
            description: feature.description,
            commands: feature
                .commands
                .into_iter()
                .enumerate()
                .map(|(index, command)| format!("{}. {command}", index + 1))
                .collect(),
            badges: Vec::new(),
        }];
        detail.subtitle = "功能详情 · 查看说明与完整命令入口".into();
        detail.footer = vec!["/帮助 返回目录；/帮助 功能序号 命令序号 查看入口。".into()];
        let mut long = detail.clone();
        long.cards[0].commands = vec![format!("/超长命令 {}", "参数说明abc-123 ".repeat(450))];
        for (name, document) in [("menu", menu), ("detail", detail), ("pagination", long)] {
            for (index, bytes) in help_png(&document, font.clone())
                .unwrap()
                .into_iter()
                .enumerate()
            {
                let file = if index == 0 {
                    format!("{name}.png")
                } else {
                    format!("{name}-{}.png", index + 1)
                };
                let image = image::load_from_memory(&bytes).unwrap();
                assert_eq!(image.width(), WIDTH);
                assert!(image.height() <= MAX_HEIGHT);
                std::fs::write(output.join(file), bytes).unwrap();
            }
        }
    }
}
