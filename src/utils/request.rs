use super::*;

use anyhow::{Context, Result, anyhow, bail};
use fancy_regex::Regex;
use futures_util::{StreamExt, stream};
use http::header::CONTENT_TYPE;
use reqwest::Url;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::Value;
use std::sync::LazyLock;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

pub const URL_LOADER: &str = "url";
pub const RECURSIVE_URL_LOADER: &str = "recursive_url";

pub const MEDIA_URL_EXTENSION: &str = "media_url";
pub const DEFAULT_EXTENSION: &str = "txt";

const MAX_CRAWLS: usize = 5;
const BREAK_ON_ERROR: bool = false;
const USER_AGENT: &str = "curl/8.6.0";

static CLIENT: LazyLock<Result<reqwest::Client>> = LazyLock::new(|| {
    let builder = reqwest::ClientBuilder::new().timeout(Duration::from_secs(16));
    let client = builder.build()?;
    Ok(client)
});

static PRESET: LazyLock<Vec<(Regex, CrawlOptions)>> = LazyLock::new(|| {
    vec![
        (
            Regex::new(r"github.com/([^/]+)/([^/]+)/tree/([^/]+)").unwrap(),
            CrawlOptions {
                exclude: vec!["changelog".into(), "changes".into(), "license".into()],
                ..Default::default()
            },
        ),
        (
            Regex::new(r"github.com/([^/]+)/([^/]+)/wiki").unwrap(),
            CrawlOptions {
                exclude: vec!["_history".into()],
                extract: Some("#wiki-body".into()),
                ..Default::default()
            },
        ),
    ]
});

static EXTENSION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.[^.]+$").unwrap());
static GITHUB_REPO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https://github\.com/([^/]+)/([^/]+)/tree/([^/]+)").unwrap());

pub async fn fetch(url: &str) -> Result<String> {
    let client = match *CLIENT {
        Ok(ref client) => client,
        Err(ref err) => bail!("{err}"),
    };
    let res = client.get(url).send().await?;
    let output = res.text().await?;
    Ok(output)
}

pub async fn fetch_with_loaders(
    loaders: &HashMap<String, String>,
    path: &str,
    allow_media: bool,
) -> Result<(String, String)> {
    if let Some(loader_command) = loaders.get(URL_LOADER) {
        let contents = run_loader_command(path, URL_LOADER, loader_command)?;
        return Ok((contents, DEFAULT_EXTENSION.into()));
    }
    let client = match *CLIENT {
        Ok(ref client) => client,
        Err(ref err) => bail!("{err}"),
    };
    let mut res = client.get(path).send().await?;
    if !res.status().is_success() {
        bail!("Invalid status: {}", res.status());
    }
    let content_type = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| match v.split_once(';') {
            Some((mime, _)) => mime.trim(),
            None => v,
        })
        .map(|v| v.to_string())
        .unwrap_or_else(|| {
            format!(
                "_/{}",
                get_patch_extension(path).unwrap_or_else(|| DEFAULT_EXTENSION.into())
            )
        });
    let mut is_media = false;
    let extension = match content_type.as_str() {
        "application/pdf" => "pdf".into(),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx".into(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx".into(),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            "pptx".into()
        }
        "application/vnd.oasis.opendocument.text" => "odt".into(),
        "application/vnd.oasis.opendocument.spreadsheet" => "ods".into(),
        "application/vnd.oasis.opendocument.presentation" => "odp".into(),
        "application/rtf" => "rtf".into(),
        "text/javascript" => "js".into(),
        "text/html" => "html".into(),
        _ => content_type
            .rsplit_once('/')
            .map(|(first, last)| {
                if ["image", "video", "audio"].contains(&first) {
                    is_media = true;
                    MEDIA_URL_EXTENSION.into()
                } else {
                    last.to_lowercase()
                }
            })
            .unwrap_or_else(|| DEFAULT_EXTENSION.into()),
    };
    let result = if is_media {
        if !allow_media {
            bail!("Unexpected media type")
        }
        if !is_supported_image_mime(&content_type) {
            bail!("Unsupported media type '{content_type}': {SUPPORTED_IMAGE_FORMATS_NOTE}")
        }
        let image_bytes = read_capped_body(res, path, MAX_IMAGE_SOURCE_BYTES).await?;
        let prepared = prepare_image_bytes_blocking(image_bytes)
            .await
            .with_context(|| format!("Invalid image '{path}'"))?;
        (prepared.to_data_url(), extension)
    } else {
        match loaders.get(&extension) {
            Some(loader_command) => {
                let save_path = temp_file("-download-", &format!(".{extension}"))
                    .display()
                    .to_string();
                let mut save_file = tokio::fs::File::create(&save_path).await?;
                let mut size = 0;
                while let Some(chunk) = res.chunk().await? {
                    size += chunk.len();
                    save_file.write_all(&chunk).await?;
                }
                let contents = if size == 0 {
                    fetch_warning_sink(JSON_OUTPUT.load(Ordering::SeqCst))
                        .println(warning_text(&format!("No content at '{path}'")));
                    String::new()
                } else {
                    run_loader_command(&save_path, &extension, loader_command)?
                };
                (contents, DEFAULT_EXTENSION.into())
            }
            None => {
                let contents = res.text().await?;
                if extension == "html" {
                    (html_to_md(&contents), "md".into())
                } else {
                    (contents, extension)
                }
            }
        }
    };
    Ok(result)
}

async fn read_capped_body(mut res: reqwest::Response, url: &str, cap: u64) -> Result<Vec<u8>> {
    let too_large = || {
        anyhow!(
            "Remote media '{url}' exceeds the {} MiB limit",
            cap / (1024 * 1024)
        )
    };
    if res.content_length().is_some_and(|len| len > cap) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        if (body.len() + chunk.len()) as u64 > cap {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub async fn fetch_models(api_base: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    let client = match *CLIENT {
        Ok(ref client) => client,
        Err(ref err) => bail!("{err}"),
    };
    let mut builder = client.get(format!("{}/models", api_base.trim_end_matches('/')));
    if let Some(api_key) = api_key {
        builder = builder.bearer_auth(api_key);
    }
    let res_body: Value = builder.send().await?.json().await?;
    let mut result: Vec<String> = res_body
        .get("data")
        .and_then(|v| v.as_array())
        .map(|v| {
            v.iter()
                .filter_map(|v| v.get("id").and_then(|v| v.as_str().map(|v| v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    if result.is_empty() {
        bail!("No valid models")
    }
    result.sort_unstable();
    Ok(result)
}

#[derive(Debug, Clone, Default)]
pub struct CrawlOptions {
    extract: Option<String>,
    exclude: Vec<String>,
    no_log: bool,
}

impl CrawlOptions {
    pub fn preset(start_url: &str) -> CrawlOptions {
        for (re, options) in PRESET.iter() {
            if let Ok(true) = re.is_match(start_url) {
                return options.clone();
            }
        }
        CrawlOptions::default()
    }
}

pub async fn crawl_website(start_url: &str, options: CrawlOptions) -> Result<Vec<Page>> {
    let start_url = Url::parse(start_url)?;
    let mut paths = vec![start_url.path().to_string()];
    let normalized_start_url = normalize_start_url(&start_url);
    if !options.no_log {
        println!(
            "Start crawling url={start_url} exclude={} extract={}",
            options.exclude.join(","),
            options.extract.as_deref().unwrap_or_default()
        );
    }

    if let Ok(true) = GITHUB_REPO_RE.is_match(start_url.as_str()) {
        paths = crawl_gh_tree(&start_url, &options.exclude)
            .await
            .with_context(|| "Failed to craw github repo".to_string())?;
    }

    let semaphore = Arc::new(Semaphore::new(MAX_CRAWLS));
    let mut result_pages = Vec::new();

    let mut index = 0;
    while index < paths.len() {
        let batch = paths[index..std::cmp::min(index + MAX_CRAWLS, paths.len())].to_vec();

        let tasks: Vec<_> = batch
            .iter()
            .map(|path| {
                let options = options.clone();
                let permit = semaphore.clone().acquire_owned(); // acquire a permit for concurrency control
                let normalized_start_url = normalized_start_url.clone();
                let path = path.clone();

                async move {
                    let _permit = permit.await?;
                    let url = normalized_start_url
                        .join(&path)
                        .map_err(|_| anyhow!("Invalid crawl page at {}", path))?;
                    let mut page = crawl_page(&normalized_start_url, &path, options)
                        .await
                        .with_context(|| format!("Failed to crawl {}", url.as_str()))?;
                    page.0 = url.as_str().to_string();
                    Ok(page)
                }
            })
            .collect();

        let results = stream::iter(tasks)
            .buffer_unordered(MAX_CRAWLS)
            .collect::<Vec<_>>()
            .await;

        let mut new_paths = Vec::new();

        for res in results {
            match res {
                Ok((path, text, links)) => {
                    if !options.no_log {
                        println!("Crawled {path}");
                    }
                    if !text.is_empty() {
                        result_pages.push(Page { path, text });
                    }
                    for link in links {
                        if !paths.iter().any(|p| match_link(p, &link)) {
                            new_paths.push(link);
                        }
                    }
                }
                Err(err) => {
                    if BREAK_ON_ERROR {
                        return Err(err);
                    } else if !options.no_log {
                        println!("{}", error_text(&pretty_error(&err)));
                    }
                }
            }
        }
        paths.extend(new_paths);

        index += batch.len();
    }

    Ok(result_pages)
}

#[derive(Debug, Deserialize)]
pub struct Page {
    pub path: String,
    pub text: String,
}

async fn crawl_gh_tree(start_url: &Url, exclude: &[String]) -> Result<Vec<String>> {
    let path_segs: Vec<&str> = start_url.path().split('/').collect();
    if path_segs.len() < 4 {
        bail!("Invalid gh tree {}", start_url.as_str());
    }
    let client = match *CLIENT {
        Ok(ref client) => client,
        Err(ref err) => bail!("{err}"),
    };
    let owner = path_segs[1];
    let repo = path_segs[2];
    let branch = path_segs[4];
    let root_path = path_segs[5..].join("/");

    let url = format!("https://api.github.com/repos/{owner}/{repo}/git/ref/heads/{branch}");

    let res_body: Value = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?
        .json()
        .await?;

    let sha = res_body["object"]["sha"]
        .as_str()
        .ok_or_else(|| anyhow!("Not found branch or tag"))?;

    let url = format!("https://api.github.com/repos/{owner}/{repo}/git/trees/{sha}?recursive=true");

    let res_body: Value = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?
        .json()
        .await?;
    let tree = res_body["tree"]
        .as_array()
        .ok_or_else(|| anyhow!("Invalid github repo tree"))?;
    let paths = tree
        .iter()
        .flat_map(|v| {
            let typ = v["type"].as_str()?;
            let path = v["path"].as_str()?;
            if typ == "blob"
                && (path.ends_with(".md") || path.ends_with(".MD"))
                && path.starts_with(&root_path)
                && !should_exclude_link(path, exclude)
            {
                Some(format!(
                    "https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{path}"
                ))
            } else {
                None
            }
        })
        .collect();

    Ok(paths)
}

async fn crawl_page(
    start_url: &Url,
    path: &str,
    options: CrawlOptions,
) -> Result<(String, String, Vec<String>)> {
    let client = match *CLIENT {
        Ok(ref client) => client,
        Err(ref err) => bail!("{err}"),
    };
    let location = start_url.join(path)?;
    let response = client
        .get(location.as_str())
        .header("User-Agent", USER_AGENT)
        .send()
        .await?;
    let body = response.text().await?;

    if let Ok(true) = GITHUB_REPO_RE.is_match(start_url.as_str()) {
        return Ok((path.to_string(), body, vec![]));
    }

    let mut links = HashSet::new();
    let document = Html::parse_document(&body);
    let selector = Selector::parse("a").map_err(|err| anyhow!("Invalid link selector, {}", err))?;

    for element in document.select(&selector) {
        if let Some(href) = element.value().attr("href") {
            let href = Url::parse(href).ok().or_else(|| location.join(href).ok());
            match href {
                None => continue,
                Some(href) => {
                    if href.as_str().starts_with(location.as_str())
                        && !should_exclude_link(href.path(), &options.exclude)
                    {
                        links.insert(href.path().to_string());
                    }
                }
            }
        }
    }

    let text = if let Some(selector) = &options.extract {
        let selector = Selector::parse(selector)
            .map_err(|err| anyhow!("Invalid extract selector, {}", err))?;
        document
            .select(&selector)
            .map(|v| html_to_md(&v.html()))
            .collect::<Vec<String>>()
            .join("\n\n")
    } else {
        html_to_md(&body)
    };

    Ok((path.to_string(), text, links.into_iter().collect()))
}

fn should_exclude_link(link: &str, exclude: &[String]) -> bool {
    if link.contains("#") {
        return true;
    }
    let parts: Vec<&str> = link.trim_end_matches('/').split('/').collect();
    let name = parts.last().unwrap_or(&"").to_lowercase();

    for exclude_name in exclude {
        let cond = match EXTENSION_RE.is_match(exclude_name) {
            Ok(true) => exclude_name.to_lowercase() == name.to_lowercase(),
            _ => exclude_name.to_lowercase() == EXTENSION_RE.replace(&name, "").to_lowercase(),
        };
        if cond {
            return true;
        }
    }
    false
}

fn normalize_start_url(start_url: &Url) -> Url {
    let mut start_url = start_url.clone();
    start_url.set_query(None);
    start_url.set_fragment(None);
    let new_path = match start_url.path().rfind('/') {
        Some(last_slash_index) => start_url.path()[..last_slash_index + 1].to_string(),
        None => start_url.path().to_string(),
    };
    start_url.set_path(&new_path);
    start_url
}

fn match_link(path: &str, link: &str) -> bool {
    path == link
        || path
            == link
                .trim_end_matches("/index.html")
                .trim_end_matches("/index.htm")
}

fn fetch_warning_sink(json_output: bool) -> OutputSink {
    OutputSink::pick(json_output, OutputSink::Stderr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
    use std::io::Cursor;
    use tokio::io::AsyncReadExt;

    const TEST_CAP: u64 = 1024 * 1024;

    #[test]
    fn the_no_content_warning_moves_to_stderr_under_json() {
        assert_eq!(fetch_warning_sink(false), OutputSink::Stdout);
        assert_eq!(fetch_warning_sink(true), OutputSink::Stderr);
    }

    async fn serve_raw(head: String, body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..n]);
            }
            // The client may hang up once it hits the cap, so write errors are expected.
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
        });
        format!("http://{addr}/file")
    }

    async fn serve_once(content_type: &'static str, body: Vec<u8>) -> String {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        serve_raw(head, body).await
    }

    async fn get(url: &str) -> reqwest::Response {
        reqwest::get(url).await.unwrap()
    }

    fn png_bytes() -> Vec<u8> {
        let image = RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255]));
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn jpeg_bytes() -> Vec<u8> {
        let image = RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]));
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut buf, ImageFormat::Jpeg)
            .unwrap();
        buf.into_inner()
    }

    async fn fetch_media_err(content_type: &'static str) -> String {
        let url = serve_once(content_type, b"payload".to_vec()).await;
        let err = fetch_with_loaders(&HashMap::new(), &url, true)
            .await
            .unwrap_err();
        format!("{err:#}")
    }

    #[tokio::test]
    async fn audio_url_is_unsupported() {
        assert_eq!(
            fetch_media_err("audio/mpeg").await,
            "Unsupported media type 'audio/mpeg': only PNG, JPEG, WebP and GIF images can be attached"
        );
    }

    #[tokio::test]
    async fn video_url_is_unsupported() {
        assert!(
            fetch_media_err("video/mp4")
                .await
                .starts_with("Unsupported media type 'video/mp4'")
        );
    }

    #[tokio::test]
    async fn svg_url_is_unsupported() {
        assert!(
            fetch_media_err("image/svg+xml")
                .await
                .starts_with("Unsupported media type 'image/svg+xml'")
        );
    }

    #[tokio::test]
    async fn png_url_attaches_as_data_url() {
        let bytes = png_bytes();
        let url = serve_once("image/png", bytes.clone()).await;
        let (contents, extension) = fetch_with_loaders(&HashMap::new(), &url, true)
            .await
            .unwrap();
        assert_eq!(
            contents,
            format!("data:image/png;base64,{}", base64_encode(&bytes))
        );
        assert_eq!(extension, MEDIA_URL_EXTENSION);
    }

    #[tokio::test]
    async fn non_image_bytes_labelled_png_name_the_url() {
        let url = serve_once("image/png", b"not an image".to_vec()).await;
        let err = fetch_with_loaders(&HashMap::new(), &url, true)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), format!("Invalid image '{url}'"), "{err:#}");
    }

    #[tokio::test]
    async fn media_url_rejected_when_media_not_allowed() {
        let url = serve_once("image/png", png_bytes()).await;
        let err = fetch_with_loaders(&HashMap::new(), &url, false)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Unexpected media type");
    }

    #[tokio::test]
    async fn jpeg_mime_aliases_attach_as_jpeg() {
        for content_type in ["image/jpg", "image/pjpeg"] {
            let bytes = jpeg_bytes();
            let url = serve_once(content_type, bytes.clone()).await;
            let (contents, _) = fetch_with_loaders(&HashMap::new(), &url, true)
                .await
                .unwrap();
            assert_eq!(
                contents,
                format!("data:image/jpeg;base64,{}", base64_encode(&bytes)),
                "{content_type}"
            );
        }
    }

    #[tokio::test]
    async fn capped_body_rejects_declared_length_over_cap() {
        // The body is shorter than declared, so only an early header check yields this error.
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            TEST_CAP + 1
        );
        let url = serve_raw(head, b"short".to_vec()).await;
        let err = read_capped_body(get(&url).await, &url, TEST_CAP)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Remote media '{url}' exceeds the 1 MiB limit")
        );
    }

    #[tokio::test]
    async fn capped_body_rejects_unsized_body_over_cap() {
        let head =
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n".to_string();
        let url = serve_raw(head, vec![0u8; TEST_CAP as usize + 1]).await;
        let err = read_capped_body(get(&url).await, &url, TEST_CAP)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Remote media '{url}' exceeds the 1 MiB limit")
        );
    }

    #[tokio::test]
    async fn capped_body_never_returns_more_than_cap_when_length_understates_body() {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {TEST_CAP}\r\nConnection: close\r\n\r\n"
        );
        let url = serve_raw(head, vec![0u8; TEST_CAP as usize * 2]).await;
        let body = read_capped_body(get(&url).await, &url, TEST_CAP)
            .await
            .unwrap();
        assert_eq!(body.len() as u64, TEST_CAP);
    }

    #[tokio::test]
    async fn capped_body_accepts_body_at_cap() {
        let url = serve_once("image/png", vec![0u8; TEST_CAP as usize]).await;
        let body = read_capped_body(get(&url).await, &url, TEST_CAP)
            .await
            .unwrap();
        assert_eq!(body.len() as u64, TEST_CAP);
    }
}
