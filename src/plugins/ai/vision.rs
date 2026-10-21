//! Bounded images from this message only. Resource identifiers are verified by
//! Milky; signed download URLs and provider errors never enter model context.
use crate::{
    host::Host,
    types::{Incoming, MessageKey},
};
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use image::{ImageDecoder, ImageFormat, ImageReader, codecs::jpeg::JpegEncoder};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    future::Future,
    io::{Cursor, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const MAX_IMAGES: usize = 4;
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_JPEG_BYTES: usize = 1024 * 1024;
const MAX_EDGE: u32 = 8192;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;
const OUTPUT_EDGE: u32 = 1024;
const RESOURCE_TIMEOUT: Duration = Duration::from_secs(5);
const BATCH_TIMEOUT: Duration = Duration::from_secs(16);
static DOWNLOADS: Semaphore = Semaphore::const_new(2);
static DECODERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));

#[derive(Clone, Default)]
pub(crate) struct Images {
    resources: Vec<String>,
    skipped: usize,
    invalid: usize,
}

impl Images {
    pub(crate) fn from_message(message: &Incoming) -> Self {
        let mut result = Self::default();
        let mut seen = HashSet::new();
        for segment in message.raw_message["segments"]
            .as_array()
            .into_iter()
            .flatten()
            .take(512)
        {
            if segment["type"] != "image" {
                continue;
            }
            let Some(resource) = segment["data"]["resource_id"].as_str().filter(|id| {
                !id.is_empty() && id.len() <= 4096 && !id.chars().any(char::is_control)
            }) else {
                result.invalid += 1;
                continue;
            };
            if !seen.insert(resource) {
                continue;
            }
            if result.resources.len() == MAX_IMAGES {
                result.skipped += 1;
            } else {
                result.resources.push(resource.to_owned());
            }
        }
        result
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.skipped == 0 && self.invalid == 0
    }

    /// Keep only usable resource IDs while a message waits in the trigger queue.
    /// One empty image records that other images could not be retained. It grants
    /// no resource access and survives another from_message pass as a fixed note.
    pub(crate) fn raw_snapshot(&self) -> Value {
        let mut segments: Vec<_> = self
            .resources
            .iter()
            .map(|id| json!({"type":"image","data":{"resource_id":id}}))
            .collect();
        if self.skipped > 0 || self.invalid > 0 {
            segments.push(json!({"type":"image","data":{}}));
        }
        json!({"segments":segments})
    }
}

#[derive(Default)]
pub(crate) struct Loaded {
    pub images: Vec<String>,
    pub note: String,
}

pub(crate) async fn load(
    host: &Host,
    key: &MessageKey,
    sources: &Images,
    cancel: &CancellationToken,
) -> Result<Loaded> {
    ensure!(!cancel.is_cancelled(), "图片处理已取消");
    if sources.is_empty() {
        return Ok(Loaded::default());
    }
    let generation = host.milky.scope_generation(&key.scope)?;
    let loaded = collect(sources, cancel, |resource| async move {
        host.milky.check_generation(generation)?;
        let url = host.milky.get_resource_temp_url(key, &resource).await?;
        let bytes = download(&url).await?;
        host.milky.check_generation(generation)?;
        Ok(bytes)
    })
    .await?;
    ensure!(!cancel.is_cancelled(), "图片处理已取消");
    host.milky.check_generation(generation)?;
    Ok(loaded)
}

// The injectable operation lets tests exercise partial failure, cancellation
// and bounded collection without a production exception for private hosts.
async fn collect<F, Fut>(
    sources: &Images,
    cancel: &CancellationToken,
    mut fetch: F,
) -> Result<Loaded>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let mut loaded = Loaded::default();
    let mut unreadable = sources.skipped > 0 || sources.invalid > 0;
    let work = async {
        for resource in &sources.resources {
            let read_image = async {
                let bytes = fetch(resource.clone()).await?;
                decode(bytes, cancel).await
            };
            match tokio::time::timeout(RESOURCE_TIMEOUT, read_image).await {
                Ok(Ok(image)) => loaded.images.push(image),
                _ => unreadable = true,
            }
        }
    };
    let timed_out = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("图片处理已取消"),
        result = tokio::time::timeout(BATCH_TIMEOUT, work) => result.is_err(),
    };
    unreadable |= timed_out;
    ensure!(!cancel.is_cancelled(), "图片处理已取消");
    if unreadable {
        loaded.note = if loaded.images.is_empty() {
            "当前消息的图片未能读取，请说明这一点，不要猜测图片内容。"
        } else {
            "当前消息的部分图片未能读取；附件按成功读取顺序排列，不能据此确定原消息中的图片编号。仅根据实际提供的图片回答，不要猜测未提供的内容。"
        }
        .into();
    }
    Ok(loaded)
}

fn image_url(value: &str) -> Result<reqwest::Url> {
    crate::types::validate_media_uri(value)?;
    let url = reqwest::Url::parse(value)?;
    let name = url.host_str().unwrap_or("").to_ascii_lowercase();
    ensure!(
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && ["qpic.cn", "qq.com", "qq.com.cn", "qlogo.cn"]
                .iter()
                .any(|suffix| name == *suffix || name.ends_with(&format!(".{suffix}"))),
        "图片下载地址不受支持"
    );
    Ok(url)
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(matches!(a, 0 | 10 | 127 | 224..=255)
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && matches!(c, 0 | 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && matches!(b, 18 | 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_v4(mapped);
            }
            let parts = ip.segments();
            // Global unicast only, excluding special-purpose, documentation and
            // 6to4 addresses (which can encode non-public IPv4 destinations).
            parts[0] & 0xe000 == 0x2000
                && !(parts[0] == 0x2001 && parts[1] < 0x0200)
                && !(parts[0] == 0x2001 && parts[1] == 0x0db8)
                && parts[0] != 0x2002
                && !(parts[0] == 0x3fff && parts[1] < 0x1000)
        }
    }
}

async fn download(value: &str) -> Result<Vec<u8>> {
    let url = image_url(value)?;
    let name = url.host_str().context("图片缺少下载地址")?.to_owned();
    let _slot = DOWNLOADS.acquire().await?;
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 443))
        .await?
        .take(17)
        .collect();
    ensure!(
        !addresses.is_empty()
            && addresses.len() <= 16
            && addresses.iter().all(|address| public_ip(address.ip())),
        "图片下载地址不可用"
    );
    // A fresh credential-free client pins every validated DNS answer. The URL's
    // hostname remains intact for Host/SNI and certificate verification.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(RESOURCE_TIMEOUT)
        .resolve_to_addrs(&name, &addresses)
        .build()?;
    let response = client.get(url).send().await?;
    ensure!(response.status().is_success(), "图片下载失败");
    crate::milky::read_bounded(response, MAX_SOURCE_BYTES).await
}

async fn decode(bytes: Vec<u8>, cancel: &CancellationToken) -> Result<String> {
    blocking(DECODERS.clone(), cancel, move || encode_image(bytes)).await
}

async fn blocking<F>(
    slots: Arc<Semaphore>,
    cancel: &CancellationToken,
    operation: F,
) -> Result<String>
where
    F: FnOnce() -> Result<String> + Send + 'static,
{
    let permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("图片处理已取消"),
        permit = slots.acquire_owned() => permit?,
    };
    let task = tokio::task::spawn_blocking(move || {
        // The closure, not the cancelled async waiter, owns the permit.
        let _permit = permit;
        operation()
    });
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("图片处理已取消"),
        result = task => result.context("图片解码任务未完成")?,
    }
}

fn dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0
            && height > 0
            && width <= MAX_EDGE
            && height <= MAX_EDGE
            && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "图片尺寸超过限制"
    );
    Ok(())
}

struct JpegBuffer(Vec<u8>);
impl Write for JpegBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_JPEG_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("JPEG exceeds output limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_image(bytes: Vec<u8>) -> Result<String> {
    ensure!(bytes.len() <= MAX_SOURCE_BYTES, "图片超过读取上限");
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader.format().context("图片格式不受支持")?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
        ),
        "图片格式不受支持"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    // Apply limits before decoder construction, including metadata/header reads.
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    dimensions(width, height)?;
    // Match ImageReader::decode's reservation for the output pixel buffer.
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    let orientation = decoder.orientation()?;
    // From_decoder reads only the first GIF frame. Apply orientation before
    // removing metadata, so camera photographs keep their intended direction.
    let mut original = image::DynamicImage::from_decoder(decoder)?;
    original.apply_orientation(orientation);
    let small = if original.width() <= OUTPUT_EDGE && original.height() <= OUTPUT_EDGE {
        original.to_rgba8()
    } else {
        original.thumbnail(OUTPUT_EDGE, OUTPUT_EDGE).to_rgba8()
    };
    // Transparent screenshots/stickers commonly contain black RGB under their
    // transparent background. Composite on white rather than dropping alpha.
    let rgb = image::RgbImage::from_fn(small.width(), small.height(), |x, y| {
        let pixel = small.get_pixel(x, y).0;
        let alpha = u16::from(pixel[3]);
        image::Rgb(std::array::from_fn(|channel| {
            ((u16::from(pixel[channel]) * alpha + 255 * (255 - alpha) + 127) / 255) as u8
        }))
    });
    let mut output = JpegBuffer(Vec::with_capacity(MAX_JPEG_BYTES));
    JpegEncoder::new_with_quality(&mut output, 85).encode(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(output.0)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Scene, Scope};

    fn png(color: [u8; 3]) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(8, 8, image::Rgb(color))
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn jpeg_pixel(data: &str) -> [u8; 3] {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.strip_prefix("data:image/jpeg;base64,").unwrap())
            .unwrap();
        image::load_from_memory(&bytes)
            .unwrap()
            .to_rgb8()
            .get_pixel(4, 4)
            .0
    }

    fn message(raw_message: Value) -> Incoming {
        Incoming {
            key: MessageKey {
                scope: Scope {
                    bot_id: 1,
                    scene: Scene::Group,
                    peer_id: 2,
                },
                message_seq: 3,
            },
            sender_id: 4,
            sender_nickname: None,
            sender_card: None,
            text: String::new(),
            mentions: vec![],
            quote_seq: None,
            received_at: 1,
            raw_message,
            notification: None,
        }
    }

    #[test]
    fn only_bounded_unique_top_level_resources_survive_snapshot() {
        let mut segments = vec![
            json!({"type":"text","data":{"text":"https://qpic.cn/user-url"}}),
            json!({"type":"reply","data":{"segments":[{"type":"image","data":{"resource_id":"quoted"}}]}}),
            json!({"type":"forward","data":{"segments":[{"type":"image","data":{"resource_id":"forwarded"}}]}}),
        ];
        for id in ["one", "one", "two", "three", "four", "five", "six", ""] {
            segments.push(json!({"type":"image","data":{"resource_id":id,"temp_url":"https://attacker.invalid/signed"}}));
        }
        let images = Images::from_message(&message(
            json!({"segments":segments,"_qkbot_vision_skipped":500}),
        ));
        assert_eq!(images.resources, ["one", "two", "three", "four"]);
        assert_eq!((images.skipped, images.invalid), (2, 1));
        let snapshot = images.raw_snapshot();
        assert!(!snapshot.to_string().contains("temp_url"));
        let again = Images::from_message(&message(snapshot));
        assert_eq!(again.resources, images.resources);
        assert!(again.invalid > 0);
        assert!(!again.is_empty());
    }

    #[test]
    fn cdn_urls_and_dns_answers_fail_closed() {
        for value in [
            "https://multimedia.nt.qq.com.cn/download?rkey=value",
            "https://gchat.qpic.cn/a",
            "https://qlogo.cn:443/a",
        ] {
            assert!(image_url(value).is_ok(), "{value}");
        }
        for value in [
            "http://qpic.cn/a",
            "https://qpic.cn:444/a",
            "https://qpic.cn.evil.test/a",
            "https://evilqpic.cn/a",
            "https://user:pass@qq.com/a",
            "https://qpic.cn/a#fragment",
            "https://127.0.0.1/a",
            "file:///a",
        ] {
            assert!(image_url(value).is_err(), "{value}");
        }
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.2",
            "100.64.0.1",
            "169.254.169.254",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:7f00:1::1",
            "3fff::1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "{address}");
        }
        for address in [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(public_ip(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn images_are_resized_reencoded_and_bounded() {
        let original = image::RgbImage::from_pixel(1800, 600, image::Rgb([240, 30, 20]));
        let mut png = Cursor::new(Vec::new());
        original.write_to(&mut png, ImageFormat::Png).unwrap();
        let encoded = encode_image(png.into_inner()).unwrap();
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(encoded.strip_prefix("data:image/jpeg;base64,").unwrap())
            .unwrap();
        assert!(jpeg.len() <= MAX_JPEG_BYTES);
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1024, 341));
        assert!(dimensions(8193, 1).is_err());
        assert!(dimensions(8192, 8192).is_err());
        assert!(encode_image(b"not an image".to_vec()).is_err());
        let mut buffer = JpegBuffer(vec![0; MAX_JPEG_BYTES]);
        assert!(buffer.write_all(&[0]).is_err());
    }

    #[test]
    fn animated_gif_uses_only_first_frame() {
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            for color in [[240, 20, 20, 255], [20, 20, 240, 255]] {
                encoder
                    .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                        8,
                        8,
                        image::Rgba(color),
                    )))
                    .unwrap();
            }
        }
        let result = encode_image(gif).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(result.split_once(',').unwrap().1)
            .unwrap();
        let pixel = image::load_from_memory(&bytes)
            .unwrap()
            .to_rgb8()
            .get_pixel(4, 4)
            .0;
        assert!(pixel[0] > 180 && pixel[2] < 80);
    }

    #[test]
    fn transparent_black_text_and_partial_alpha_are_composited_on_white() {
        let source = image::RgbaImage::from_fn(64, 32, |x, y| {
            if (8..16).contains(&x) && (8..24).contains(&y) {
                image::Rgba([0, 0, 0, 255])
            } else if x >= 32 {
                image::Rgba([255, 0, 0, 128])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        let mut png = Cursor::new(Vec::new());
        source.write_to(&mut png, ImageFormat::Png).unwrap();
        let data = encode_image(png.into_inner()).unwrap();
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(data.split_once(',').unwrap().1)
            .unwrap();
        let pixels = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        let white = pixels.get_pixel(4, 16).0;
        let black = pixels.get_pixel(12, 16).0;
        let pink = pixels.get_pixel(48, 16).0;
        assert!(white.into_iter().all(|value| value > 240));
        assert!(black.into_iter().all(|value| value < 15));
        assert!(pink[0] > 240 && (118..=136).contains(&pink[1]) && (118..=136).contains(&pink[2]));
    }

    #[test]
    fn exif_orientation_is_applied_before_metadata_is_removed() {
        use image::ImageEncoder;

        // Little-endian TIFF IFD with one SHORT Orientation entry set to 6
        // (90 degrees clockwise). JpegEncoder adds the Exif APP1 signature.
        let exif = vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0,
            0,
        ];
        let source = image::RgbImage::from_fn(32, 16, |x, _| {
            if x < 16 {
                image::Rgb([240, 20, 20])
            } else {
                image::Rgb([20, 20, 240])
            }
        });
        let mut jpeg = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, 95);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .encode(source.as_raw(), 32, 16, image::ExtendedColorType::Rgb8)
            .unwrap();
        let data = encode_image(jpeg).unwrap();
        let result = base64::engine::general_purpose::STANDARD
            .decode(data.split_once(',').unwrap().1)
            .unwrap();
        let pixels = image::load_from_memory(&result).unwrap().to_rgb8();
        assert_eq!(pixels.dimensions(), (16, 32));
        let top = pixels.get_pixel(8, 4).0;
        let bottom = pixels.get_pixel(8, 28).0;
        assert!(top[0] > 180 && top[2] < 80);
        assert!(bottom[0] < 80 && bottom[2] > 180);
        assert!(!result.windows(4).any(|bytes| bytes == b"Exif"));
    }

    #[tokio::test]
    async fn collection_preserves_order_deduplicates_and_limits_images() {
        let segments: Vec<_> = ["red", "red", "green", "blue", "yellow", "fifth"]
            .into_iter()
            .map(|id| json!({"type":"image","data":{"resource_id":id}}))
            .collect();
        let sources = Images::from_message(&message(json!({"segments":segments})));
        let mut fetched = Vec::new();
        let loaded = collect(&sources, &CancellationToken::new(), |id| {
            fetched.push(id.clone());
            async move {
                let color = match id.as_str() {
                    "red" => [240, 20, 20],
                    "green" => [20, 240, 20],
                    "blue" => [20, 20, 240],
                    "yellow" => [240, 240, 20],
                    _ => panic!("excess image was fetched"),
                };
                Ok(png(color))
            }
        })
        .await
        .unwrap();
        assert_eq!(fetched, ["red", "green", "blue", "yellow"]);
        assert_eq!(loaded.images.len(), MAX_IMAGES);
        for (data, expected) in
            loaded
                .images
                .iter()
                .zip([[240, 20, 20], [20, 240, 20], [20, 20, 240], [240, 240, 20]])
        {
            let pixel = jpeg_pixel(data);
            assert!(
                pixel
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| actual.abs_diff(expected) < 8)
            );
        }
        assert!(loaded.note.contains("图片") && loaded.note.contains("未能读取"));
    }

    #[tokio::test]
    async fn partial_failures_are_fixed_notes_and_cancellation_is_not_downgraded() {
        let images = Images {
            resources: vec!["good".into(), "bad".into()],
            skipped: 0,
            invalid: 0,
        };
        let loaded = collect(&images, &CancellationToken::new(), |id| async move {
            if id == "bad" {
                bail!("provider secret and signed URL");
            }
            Ok(png([240, 20, 20]))
        })
        .await
        .unwrap();
        assert_eq!(loaded.images.len(), 1);
        assert!(jpeg_pixel(&loaded.images[0])[0] > 180);
        assert!(loaded.note.contains("图片") && loaded.note.contains("未能读取"));
        assert!(!loaded.note.contains("secret"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            collect(&images, &cancel, |_| async {
                panic!("cancelled operation fetched an image");
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn cancelled_decode_keeps_its_permit_until_blocking_work_finishes() {
        let slots = Arc::new(Semaphore::new(1));
        let cancel = CancellationToken::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_slots = slots.clone();
        let worker_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            blocking(worker_slots, &worker_cancel, move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                Ok("finished".into())
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
        assert!(task.await.unwrap().is_err());
        assert_eq!(slots.available_permits(), 0);
        release_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(slots.available_permits(), 1);
    }
}
