use anyhow::{bail, Context, Result};
#[cfg(feature = "oss")]
use {
    aws_config::meta::region::RegionProviderChain, aws_config::BehaviorVersion,
    aws_sdk_s3::config::Region, aws_sdk_s3::primitives::ByteStream,
};
use chrono::{DateTime, Utc};
use chrono_tz::Asia::Tokyo;
use clap::Parser;
use futures::StreamExt;
use image::{
    codecs::{avif::AvifEncoder, jpeg::JpegEncoder, webp::WebPEncoder},
    imageops, GenericImageView, ImageFormat, RgbaImage,
};
use mozjpeg::{ColorSpace, Compress};
use reqwest::header::{self, HeaderMap};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::Mutex;
use tokio_stream;
use url::Url;

// --- 命令行参数定义 ---
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    #[clap(short, long, value_name = "FILE", default_value = "config.toml")]
    config: PathBuf,
    #[clap(short, long)]
    daemon: bool,
    #[clap(long, value_name = "FILE_PATH")]
    log: Option<Option<PathBuf>>,
}

// --- 配置文件结构 ---
#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum ImageFormatType { Webp, Jpeg, Mozjpeg, Avif, }

#[derive(Deserialize, Debug, Clone, Copy)]
struct ImageOutputConfig {
    format: ImageFormatType,
    quality: Option<u8>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
struct Config {
    download_concurrent_limit: usize,
    task_concurrent_limit: usize,
    subscriptions: Vec<String>,
    cookie: Option<String>,
    #[serde(default)]
    daemon: Option<DaemonConfig>,
    output: StorageConfig,
    image_output: Option<ImageOutputConfig>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
struct DaemonConfig {
    interval_seconds: u64,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StorageConfig {
    Local { path: String, },
    #[cfg(feature = "oss")]
    S3 {
        endpoint: String,
        bucket: String,
        region: String,
        access_key_id: String,
        secret_access_key: String,
        base_path: Option<String>,
    },
}

// 对象存储
enum StorageClient {
    Local { root: PathBuf, },
    #[cfg(feature = "oss")]
    S3 {
        client: aws_sdk_s3::Client,
        bucket: String,
        base_path: String,
    },
}

impl StorageClient {
    async fn new(config: &StorageConfig) -> Result<Self> {
        match config {
            StorageConfig::Local { path } => Ok(StorageClient::Local { root: PathBuf::from(path), }),
            #[cfg(feature = "oss")]
            StorageConfig::S3 { endpoint, bucket, region, access_key_id, secret_access_key, base_path, } => {
                let region_provider = RegionProviderChain::first_try(Region::new(region.clone()));
                let creds = aws_sdk_s3::config::Credentials::new(access_key_id, secret_access_key, None, None, "Static");
                let sdk_config = aws_config::defaults(BehaviorVersion::latest())
                    .region(region_provider).credentials_provider(creds).endpoint_url(endpoint).load().await;
                let client = aws_sdk_s3::Client::new(&sdk_config);
                println!("[信息] S3/OSS 客户端初始化成功，目标存储桶: {}", bucket);
                Ok(StorageClient::S3 { client, bucket: bucket.clone(), base_path: base_path.clone().unwrap_or_default(), })
            }
        }
    }

    async fn save(&self, key: &Path, data: Vec<u8>) -> Result<()> {
        match self {
            StorageClient::Local { root } => {
                let full_path = root.join(key);
                if let Some(parent) = full_path.parent() { fs::create_dir_all(parent).await?; }
                fs::write(&full_path, data).await?;
                Ok(())
            }
            #[cfg(feature = "oss")]
            StorageClient::S3 { client, bucket, base_path, } => {
                let full_key = Path::new(base_path).join(key);
                let stream = ByteStream::from(data);
                client.put_object().bucket(bucket).key(full_key.to_string_lossy()).body(stream).send().await?;
                Ok(())
            }
        }
    }

    async fn read_to_vec(&self, key: &Path) -> Result<Option<Vec<u8>>> {
        match self {
            StorageClient::Local { root } => {
                let full_path = root.join(key);
                if !fs::try_exists(&full_path).await? { return Ok(None); }
                Ok(Some(fs::read(&full_path).await?))
            }
            #[cfg(feature = "oss")]
            StorageClient::S3 { client, bucket, base_path, } => {
                let full_key = Path::new(base_path).join(key);
                match client.get_object().bucket(bucket).key(full_key.to_string_lossy()).send().await {
                    Ok(resp) => Ok(Some(resp.body.collect().await?.into_bytes().to_vec())),
                    Err(sdk_err) => match sdk_err.into_service_error() {
                        e if e.is_no_such_key() => Ok(None),
                        e => Err(e.into()),
                    },
                }
            }
        }
    }
}

// --- Kuragebunch API 数据结构 ---
#[derive(Deserialize, Debug, Clone)]
struct EpisodeInfo {
    readable_product_id: String,
    title: String,
    viewer_uri: String,
    status: EpisodeStatus,
}

#[derive(Deserialize, Debug, Clone)]
struct EpisodeStatus {
    label: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct SeriesInfo {
    id: String,
    title: String,
    author: String,
    description: String,
    cover_url: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct EpisodeJsonData {
    readable_product: ReadableProduct,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ReadableProduct {
    series: SeriesMeta,
    page_structure: PageStructure,
}

#[derive(Deserialize, Debug)]
struct SeriesMeta {
    id: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PageStructure {
    cho_ju_giga: String,
    pages: Vec<Page>,
}

#[derive(Deserialize, Debug, Clone)]
struct Page {
    #[serde(rename = "type")]
    page_type: String,
    src: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct DownloadLog {
    series_id: String,
    series_title: String,
    chapters: HashMap<String, ChapterLog>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ChapterLog {
    title: String,
    completed: bool,
}

// --- 图像处理和辅助函数 ---
fn restore_image(scrambled_data: &[u8]) -> Result<RgbaImage> {
    let scrambled_img = image::load_from_memory_with_format(scrambled_data, ImageFormat::Jpeg)?.to_rgba8();
    let (width, height) = scrambled_img.dimensions();
    let mut restored_image = RgbaImage::new(width, height);

    const DIVIDE_NUM: u32 = 4;
    const MULTIPLE: u32 = 8;
    let cell_width = (width / (DIVIDE_NUM * MULTIPLE)) * MULTIPLE;
    let cell_height = (height / (DIVIDE_NUM * MULTIPLE)) * MULTIPLE;

    if cell_width == 0 || cell_height == 0 { bail!("计算出的单元格尺寸为零，无法还原图片。"); }

    for source_index in 0..(DIVIDE_NUM * DIVIDE_NUM) {
        let source_row = source_index / DIVIDE_NUM;
        let source_col = source_index % DIVIDE_NUM;
        let dest_index = source_col * DIVIDE_NUM + source_row;
        let dest_row = dest_index / DIVIDE_NUM;
        let dest_col = dest_index % DIVIDE_NUM;
        let src_x = source_col * cell_width;
        let src_y = source_row * cell_height;
        let dest_x = dest_col * cell_width;
        let dest_y = dest_row * cell_height;
        let block_view = scrambled_img.view(src_x, src_y, cell_width, cell_height);
        imageops::replace(&mut restored_image, &block_view.to_image(), dest_x.into(), dest_y.into());
    }
    Ok(restored_image)
}

fn encode_image(img: &RgbaImage, config: Option<&ImageOutputConfig>) -> Result<(Vec<u8>, &'static str)> {
    let (format, quality_opt) = config.map_or((ImageFormatType::Webp, None), |c| (c.format, c.quality));
    match format {
        ImageFormatType::Webp => { let mut buffer = std::io::Cursor::new(Vec::new()); img.write_with_encoder(WebPEncoder::new_lossless(&mut buffer))?; Ok((buffer.into_inner(), "webp")) },
        ImageFormatType::Jpeg => { let mut buffer = std::io::Cursor::new(Vec::new()); let quality = quality_opt.unwrap_or(90).max(0).min(100); let rgb_img = image::DynamicImage::ImageRgba8(img.clone()).into_rgb8(); rgb_img.write_with_encoder(JpegEncoder::new_with_quality(&mut buffer, quality))?; Ok((buffer.into_inner(), "jpg")) },
        ImageFormatType::Mozjpeg => { let quality = quality_opt.unwrap_or(90).max(0).min(100) as f32; let (width, height) = img.dimensions(); let rgb_pixels: Vec<u8> = img.pixels().flat_map(|p| [p[0], p[1], p[2]]).collect(); let mut comp = Compress::new(ColorSpace::JCS_RGB); comp.set_size(width as usize, height as usize); comp.set_quality(quality); let mut comp_started = comp.start_compress(Vec::new())?; comp_started.write_scanlines(&rgb_pixels)?; let jpeg_data = comp_started.finish()?; Ok((jpeg_data, "jpg")) },
        ImageFormatType::Avif => { let mut buffer = std::io::Cursor::new(Vec::new()); let quality = quality_opt.unwrap_or(80).max(0).min(100); let speed = 8; img.write_with_encoder(AvifEncoder::new_with_speed_quality(&mut buffer, speed, 100 - quality))?; Ok((buffer.into_inner(), "avif")) }
    }
}

fn sanitize_filename(name: &str) -> String { name.replace(&['/', '\\', ':', '*', '?', '"', '<', '>', '|'][..], "_") }

fn strip_html(html: &str) -> String {
    let text_with_newlines = regex::Regex::new(r"(?i)<br\s*/?>").unwrap().replace_all(html, "\n");
    let without_tags = regex::Regex::new(r"<[^>]*>").unwrap().replace_all(&text_with_newlines, "");
    regex::Regex::new(r"[ \t\r\n]+").unwrap().replace_all(without_tags.trim(), " ").to_string()
}

// 从 cdn-scissors URL 中提取并解码出原始图片 URL
fn extract_original_image_url(cdn_url: &str) -> String {
    // 原始 URL 通常是最后一个 '/' 之后的部分，并经过了 URL 编码
    if let Some(encoded_part) = cdn_url.rsplit('/').next() {
        if let Ok(decoded) = urlencoding::decode(encoded_part) {
            // 简单验证解码后的字符串是否像一个 URL
            if decoded.starts_with("http") {
                return decoded.into_owned();
            }
        }
    }
    // 如果解析失败，返回原始的 cdn_url 以避免程序崩溃
    cdn_url.to_string()
}

// 从任意一话的 HTML 中获取系列元数据和 ID
async fn get_series_metadata(http_client: &reqwest::Client, episode_uri: &str) -> Result<SeriesInfo> {
    let html_content = http_client.get(episode_uri).send().await?.text().await?;
    let document = Html::parse_document(&html_content);

    let script_selector = Selector::parse("script#episode-json").unwrap();
    let script_tag = document.select(&script_selector).next().context("在章节页面未找到 'episode-json' script 标签")?;
    let json_str = script_tag.attr("data-value").context("'data-value' 属性不存在")?;
    let episode_data: EpisodeJsonData = serde_json::from_str(json_str)?;
    let aggregate_id = episode_data.readable_product.series.id;

    let series_title = document.select(&Selector::parse("h1.series-header-title").unwrap()).next().map(|e| e.inner_html()).unwrap_or_else(|| "未知系列".to_string());
    let series_author = document.select(&Selector::parse("h2.series-header-author").unwrap()).next().map(|e| e.inner_html()).unwrap_or_else(|| "未知作者".to_string());
    let series_desc_html = document.select(&Selector::parse("p.series-header-description").unwrap()).next().map(|e| e.inner_html()).unwrap_or_default();
    let complex_cover_url = document.select(&Selector::parse("img.js-series-header-image").unwrap()).next().and_then(|e| e.value().attr("data-src")).unwrap_or_default().to_string();
    
    // 调用辅助函数清洗封面 URL
    let cover_url = extract_original_image_url(&complex_cover_url);

    Ok(SeriesInfo {
        id: aggregate_id,
        title: strip_html(&series_title),
        author: strip_html(&series_author),
        description: strip_html(&series_desc_html),
        cover_url,
    })
}

// 下载单个章节
async fn download_episode(storage: &StorageClient, http_client: &reqwest::Client, semaphore: Arc<tokio::sync::Semaphore>, episode_uri: &str, episode_title: &str, base_key: &Path, image_output_config: Option<ImageOutputConfig>) -> Result<bool> {
    let episode_html = http_client.get(episode_uri).send().await?.text().await?;
    let document = Html::parse_document(&episode_html);

    let script_selector = Selector::parse("script#episode-json").unwrap();
    let script_tag = document.select(&script_selector).next().context("未能找到 'episode-json' script 标签")?;
    let json_str = script_tag.attr("data-value").context("'data-value' 属性不存在")?;
    let episode_data: EpisodeJsonData = serde_json::from_str(json_str)?;
    
    let page_structure = episode_data.readable_product.page_structure;
    let pages_to_download: Vec<Page> = page_structure.pages.into_iter().filter(|p| p.page_type == "main" && p.src.is_some()).collect();
    
    let expected_page_count = pages_to_download.len();
    println!("[处理] '{}' 共有 {} 页。", episode_title, expected_page_count);
    if expected_page_count == 0 { return Ok(true); }

    let successful_saves = Arc::new(Mutex::new(0));
    let errors = Arc::new(Mutex::new(Vec::new()));
    
    let page_stream = tokio_stream::iter(pages_to_download.into_iter().enumerate());
    page_stream.for_each_concurrent(None, |(i, page)| {
        let http_client = http_client.clone();
        let base_key = base_key.to_path_buf();
        let successful_saves = Arc::clone(&successful_saves);
        let errors = Arc::clone(&errors);
        let semaphore = Arc::clone(&semaphore);
        let storage = &*storage;
        let cho_ju_giga = page_structure.cho_ju_giga.clone();

        async move {
            let task = async {
                let page_num = i + 1;
                let image_url = page.src.context("页面缺少 src URL")?;
                let _permit = semaphore.acquire().await.unwrap();
                let scrambled_bytes = http_client.get(&image_url).send().await?.bytes().await?;

                let (img_bytes, extension) = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, &'static str)> {
                    let final_image = if cho_ju_giga == "baku" {
                        restore_image(&scrambled_bytes)?
                    } else {
                        image::load_from_memory(&scrambled_bytes)?.to_rgba8()
                    };
                    encode_image(&final_image, image_output_config.as_ref())
                }).await??;

                let file_key = base_key.join(format!("{:03}.{}", page_num, extension));
                storage.save(&file_key, img_bytes).await?;
                
                let mut count = successful_saves.lock().await; *count += 1;
                Ok::<(), anyhow::Error>(())
            };
            if let Err(e) = task.await { let mut errors_guard = errors.lock().await; errors_guard.push(e); }
        }
    }).await;

    let final_count = *successful_saves.lock().await;
    let final_errors = errors.lock().await;
    if !final_errors.is_empty() { 
        eprintln!("[验证失败] '{}' 下载出现错误 (成功 {} / 预期 {})。", episode_title, final_count, expected_page_count); 
        final_errors.iter().for_each(|e| eprintln!("  - 失败原因: {}", e)); 
        return Ok(false); 
    }
    if final_count != expected_page_count { 
        eprintln!("[验证失败] '{}' 下载未完成 (成功 {} / 预期 {})。", episode_title, final_count, expected_page_count); 
        return Ok(false); 
    }
    println!("[验证成功] '{}' 所有页码已下载。", episode_title);
    Ok(true)
}

// --- 主任务循环 ---
async fn run_tasks(config: &Config, http_client: &reqwest::Client, storage: &Arc<StorageClient>) -> Result<()> {
    println!("--- 开始执行 Kuragebunch 下载任务 ---");
    let download_semaphore = Arc::new(tokio::sync::Semaphore::new(
        if config.download_concurrent_limit == 0 {
            tokio::sync::Semaphore::MAX_PERMITS
        } else {
            config.download_concurrent_limit
        },
    ));
  
    for keyword in &config.subscriptions {
        println!("\n========================================\n处理订阅关键词: '{}'\n========================================", keyword);
      
        let search_url = format!("https://kuragebunch.com/search?q={}", url::form_urlencoded::byte_serialize(keyword.as_bytes()).collect::<String>());
        let search_html = http_client.get(&search_url).send().await?.text().await?;
        let document = Html::parse_document(&search_html);
      
        let result_selector = Selector::parse("li[test-result-readable-product]").unwrap();
        let results: Vec<_> = document.select(&result_selector).collect();

        if results.is_empty() { println!("[警告] 关键词 '{}' 没有找到任何结果。", keyword); continue; }
        
        // 预处理，统计每个标题的出现次数以处理重名问题
        let mut title_counts: HashMap<String, usize> = HashMap::new();
        for result in &results {
            let title_selector = Selector::parse(".series-title").unwrap();
            if let Some(title_element) = result.select(&title_selector).next() {
                let title = title_element.inner_html().trim().to_string();
                *title_counts.entry(title).or_insert(0) += 1;
            }
        }
        
        println!("[信息] 关键词 '{}' 找到 {} 个初步结果，开始逐一处理...", keyword, results.len());

        // 对每个搜索结果独立处理
        for result in results {
            let title_selector = Selector::parse(".series-title").unwrap();
            let result_title = result.select(&title_selector).next().map(|t| t.inner_html().trim().to_string()).unwrap_or_default();
            
            // 确保只处理与关键词完全匹配的系列
            if result_title != *keyword { continue; }

            let link_selector = Selector::parse("a.sub-link, a.main-link").unwrap();
            if let Some(episode_path) = result.select(&link_selector).next().and_then(|a| a.value().attr("href")) {
                let full_episode_uri = episode_path.to_string();
                
                let series_info = match get_series_metadata(http_client, &full_episode_uri).await {
                    Ok(meta) => meta,
                    Err(e) => {
                        eprintln!("[警告] 无法从 '{}' 获取系列元数据: {}", full_episode_uri, e);
                        continue;
                    }
                };
                
                // 根据标题是否重名来决定文件夹路径
                let is_duplicate = title_counts.get(&series_info.title).cloned().unwrap_or(1) > 1;
                let manga_key = if is_duplicate {
                    PathBuf::from(format!(
                        "{} [{}]",
                        sanitize_filename(&series_info.title),
                        &series_info.id
                    ))
                } else {
                    PathBuf::from(sanitize_filename(&series_info.title))
                };
                
                println!(">>> 处理系列: '{}' (ID: {})", series_info.title, series_info.id);

                let api_url = format!("https://kuragebunch.com/api/viewer/pagination_readable_products?type=episode&aggregate_id={}&offset=0&limit=100&sort_order=desc&is_guest=1", series_info.id);
                
                let episodes = match http_client.get(&api_url).send().await {
                    Ok(res) => match res.json::<Vec<EpisodeInfo>>().await {
                        Ok(eps) => eps.into_iter().filter(|ep| ep.status.label == "is_free").collect::<Vec<_>>(),
                        Err(e) => {
                            eprintln!("[错误] 解析系列ID '{}' 的章节JSON失败: {}", series_info.id, e);
                            continue;
                        }
                    },
                    Err(e) => {
                        eprintln!("[错误] 请求系列ID '{}' 的章节列表失败: {}", series_info.id, e);
                        continue;
                    }
                };

                if episodes.is_empty() {
                    println!("[信息] 系列 '{}' (ID: {}) 未找到可下载的免费章节。", series_info.title, series_info.id);
                    continue;
                }
                println!("[信息] 找到 {} 个免费章节。", episodes.len());
                
                let info_key = manga_key.join("info.json");
                if storage.read_to_vec(&info_key).await?.is_none() {
                    storage.save(&info_key, serde_json::to_string_pretty(&series_info)?.into_bytes()).await?;
                }

                if !series_info.cover_url.is_empty() {
                    let cover_url = &series_info.cover_url;
                    let cover_path = Url::parse(cover_url)?.path().to_string();
                    let cover_ext = Path::new(&cover_path).extension().and_then(|s| s.to_str()).unwrap_or("jpg");
                    let cover_key = manga_key.join(format!("cover.{}", cover_ext));
                    
                    if storage.read_to_vec(&cover_key).await?.is_none() {
                        println!("[下载] 封面 (ID: {})...", series_info.id);
                        if let Ok(res) = http_client.get(cover_url).send().await {
                           storage.save(&cover_key, res.bytes().await?.to_vec()).await?;
                        }
                    }
                }
                
                let log_key = manga_key.join("download_log.json");
                let log_data = storage.read_to_vec(&log_key).await?;
                let log = Arc::new(Mutex::new(if let Some(data) = log_data {
                    serde_json::from_slice(&data).unwrap_or_default()
                } else {
                    DownloadLog { series_id: series_info.id.clone(), series_title: series_info.title.clone(), chapters: HashMap::new() }
                }));

                let chapter_tasks: Vec<_> = episodes.into_iter().map(|episode| {
                    let storage = Arc::clone(&storage);
                    let http_client = http_client.clone();
                    let manga_key = manga_key.clone();
                    let log = Arc::clone(&log);
                    let download_semaphore = Arc::clone(&download_semaphore);
                    let image_output_config = config.image_output;

                    async move {
                        let episode_id_str = episode.readable_product_id.clone();
                        if let Some(chapter_log) = log.lock().await.chapters.get(&episode_id_str) { 
                            if chapter_log.completed { println!("[跳过] '{}' 已在日志中标记完成。", episode.title); return; } 
                        }
                        
                        println!("[任务] 开始处理 '{}'", episode.title);
                        let episode_base_key = manga_key.join(sanitize_filename(&episode.title));
                        
                        match download_episode(&storage, &http_client, Arc::clone(&download_semaphore), &episode.viewer_uri, &episode.title, &episode_base_key, image_output_config).await {
                            Ok(true) => { 
                                let mut lg = log.lock().await; 
                                lg.chapters.insert(episode_id_str, ChapterLog { title: episode.title.clone(), completed: true }); 
                            }
                            Err(e) => eprintln!("[错误] 处理 '{}' 失败: {}", episode.title, e),
                            _ => {}
                        }
                    }
                }).collect();

                let task_limit = if config.task_concurrent_limit == 0 { None } else { Some(config.task_concurrent_limit) };
                tokio_stream::iter(chapter_tasks).for_each_concurrent(task_limit, |task| task).await;

                println!("[日志] 正在将下载记录写回存储 (ID: {})...", series_info.id);
                storage.save(&log_key, serde_json::to_string_pretty(&*log.lock().await)?.into_bytes()).await?;
            }
        }
    }
    println!("\n--- 所有任务执行完毕！ ---");
    Ok(())
}


// --- 守护进程和主函数入口 ---
#[cfg(unix)]
async fn daemon_main_loop(config_path: PathBuf) -> Result<()> {
    let config_str = fs::read_to_string(&config_path).await?;
    let config: Config = toml::from_str(&config_str)?;
    let interval = config.daemon.as_ref().context("配置文件中缺少 [daemon] 部分")?.interval_seconds;
    let storage = Arc::new(StorageClient::new(&config.output).await?);
    let http_client = build_http_client(&config.cookie)?;
    loop {
        println!("\n--- [{}] 开始新一轮检查 ---", Utc::now().with_timezone(&Tokyo).to_rfc2822());
        if let Err(e) = run_tasks(&config, &http_client, &storage).await { eprintln!("[错误] 守护进程任务执行失败: {}", e); }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

fn build_http_client(cookie: &Option<String>) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0".parse()?);
    headers.insert(header::REFERER, "https://kuragebunch.com/".parse()?);
    if let Some(c) = cookie { headers.insert(header::COOKIE, c.parse()?); }
    Ok(reqwest::Client::builder().default_headers(headers).timeout(Duration::from_secs(60)).build()?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_str = fs::read_to_string(&cli.config).await?;
    let config: Config = toml::from_str(&config_str)?;

    if cli.daemon {
        #[cfg(unix)] {
            if config.daemon.is_none() { bail!("已通过命令行启用守护进程模式，但配置文件 '{}' 中缺少 [daemon] 配置节。", cli.config.to_string_lossy()); }
            let default_log_path = std::env::temp_dir().join("kuragebunch_dl.log");
            let log_file = match cli.log { Some(Some(path)) => std::fs::File::create(path)?, Some(None) => std::fs::File::create(default_log_path)?, None => std::fs::File::create("/dev/null")?, };
            let stderr = log_file.try_clone()?;
            let daemonize = daemonize::Daemonize::new().pid_file("/tmp/.kuragebunch_dl.pid").working_directory("/").stdout(log_file).stderr(stderr);
            match daemonize.start() {
                Ok(_) => { println!("已进入守护进程模式。"); if let Err(e) = daemon_main_loop(cli.config).await { eprintln!("守护进程主循环异常退出: {}", e); } }
                Err(e) => eprintln!("守护进程启动失败: {}", e),
            }
        }
        #[cfg(not(unix))] { bail!("守护进程模式 (-d, --daemon) 仅在 Unix-like 系统上受支持。"); }
    } else {
        let storage = Arc::new(StorageClient::new(&config.output).await?);
        let http_client = build_http_client(&config.cookie)?;
        run_tasks(&config, &http_client, &storage).await?;
    }
    Ok(())
}
