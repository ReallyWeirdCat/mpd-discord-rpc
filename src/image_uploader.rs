use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rand::seq::SliceRandom;
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use tokio::task::spawn_blocking;

use tracing::debug;

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

pub struct ImageUploader {
    cache: Mutex<HashMap<PathBuf, String>>,
    user_agents: Vec<String>,
    current_ua: Mutex<usize>,
}

impl ImageUploader {
    pub fn new() -> Self {
        let user_agents = vec![
            APP_USER_AGENT.to_string(),
            // "curl/8.13.0".to_string(),
            // "PostmanRuntime/7.32.0".to_string(),
            // "Wget/1.21.3".to_string(),
            // "HTTPie/3.2.2".to_string(),
            // "insomnia/2023.5.8".to_string(),
            // "Python-urllib/3.11".to_string(),
        ];
        let mut shuffled = user_agents.clone();
        shuffled.shuffle(&mut rand::thread_rng());

        Self {
            cache: Mutex::new(HashMap::new()),
            user_agents: shuffled,
            current_ua: Mutex::new(0),
        }
    }

    pub async fn upload_local_file(&self, path: &Path) -> Option<String> {
        {
            let cache = self.cache.lock().unwrap();
            if let Some(url) = cache.get(path) {
                tracing::debug!("Cache hit for {}", path.display());
                return Some(url.clone());
            }
        }
        debug!("Preparing to upload {}", path.display());

        let (bytes, mime) = spawn_blocking({
            let path = path.to_path_buf();
            move || extract_picture_from_file(&path)
        })
        .await
        .ok()??;

        let ext = mime_to_extension(&mime).unwrap_or("jpg");
        let filename = format!(
            "cover_{}.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            ext
        );

        let url = self.upload_bytes(&bytes, &filename).await?;

        self.cache
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), url.clone());
        debug!("Cached {} ({})", path.display(), url);
        Some(url)
    }

    async fn upload_bytes(&self, bytes: &[u8], filename: &str) -> Option<String> {
        let ua = self.get_next_user_agent();
        let client = Client::builder().user_agent(&ua).build().ok()?;

        // Try litterbox.catbox.moe
        if bytes.len() <= 200 * 1024 * 1024
            && let Some(url) = Self::upload_to_litterbox(&client, bytes, filename, "1h").await
        {
            return Some(url);
        }

        // Try uguu.se
        if bytes.len() <= 128 * 1024 * 1024
            && let Some(url) = Self::upload_to_uguu(&client, bytes, filename).await
        {
            return Some(url);
        }

        // Try tmpfiles.org
        if bytes.len() <= 100 * 1024 * 1024
            && let Some(url) = Self::upload_to_tmpfiles(&client, bytes, filename).await
        {
            return Some(url);
        }

        None
    }

    async fn upload_to_litterbox(
        client: &Client,
        bytes: &[u8],
        filename: &str,
        time: &str,
    ) -> Option<String> {
        let form = Form::new()
            .text("reqtype", "fileupload")
            .text("time", time.to_string())
            .text("fileNameLength", filename.len().to_string())
            .part(
                "fileToUpload",
                Part::bytes(bytes.to_vec()).file_name(filename.to_string()),
            );

        let resp = client
            .post("https://litterbox.catbox.moe/resources/internals/api.php")
            .multipart(form)
            .send()
            .await
            .ok()?;
        debug!("litterbox.catbox.moe response: {:?}", resp);

        let text = resp.text().await.ok()?;

        if text.starts_with("http") {
            let url = Some(text.trim().to_string());
            debug!("Using URL from litter.catbox.moe: {:?}", url);
            url
        } else {
            None
        }
    }

    async fn upload_to_uguu(client: &Client, bytes: &[u8], filename: &str) -> Option<String> {
        let form = Form::new().part(
            "files[]",
            Part::bytes(bytes.to_vec()).file_name(filename.to_string()),
        );
        let resp = client
            .post("https://uguu.se/upload")
            .multipart(form)
            .send()
            .await
            .ok()?;
        debug!("uguu.se response: {:?}", resp);
        #[derive(Deserialize)]
        struct UguuResponse {
            success: bool,
            files: Vec<UguuFile>,
        }
        #[derive(Deserialize)]
        struct UguuFile {
            url: String,
        }
        let json: UguuResponse = resp.json().await.ok()?;
        if json.success && !json.files.is_empty() {
            let url = json.files[0].url.clone();
            debug!("Using URL from uguu.se: {:?}", url);
            Some(url)
        } else {
            None
        }
    }

    async fn upload_to_tmpfiles(client: &Client, bytes: &[u8], filename: &str) -> Option<String> {
        let form = Form::new().part(
            "file",
            Part::bytes(bytes.to_vec()).file_name(filename.to_string()),
        );
        let resp = client
            .post("https://tmpfiles.org/api/v1/upload")
            .multipart(form)
            .send()
            .await
            .ok()?;
        debug!("tmpfiles.org response: {:?}", resp);
        #[derive(Deserialize)]
        struct TmpResponse {
            status: String,
            data: Option<TmpData>,
        }
        #[derive(Deserialize)]
        struct TmpData {
            url: String,
        }
        let json: TmpResponse = resp.json().await.ok()?;
        if json.status == "success" {
            let url = json.data.map(|d| {
                d.url
                    .replace("https://tmpfiles.org/", "https://tmpfiles.org/dl/")
            });
            debug!("Using URL from tmpfiles.org: {:?}", url);
            url
        } else {
            None
        }
    }

    fn get_next_user_agent(&self) -> String {
        let mut idx = self.current_ua.lock().unwrap();
        let ua = self.user_agents[*idx].clone();
        *idx = (*idx + 1) % self.user_agents.len();
        ua
    }
}

fn mime_to_extension(mime: &str) -> Option<&'static str> {
    match mime {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

fn extract_picture_from_file(path: &Path) -> Option<(Vec<u8>, String)> {
    use lofty::file::TaggedFileExt;
    let tagged_file = lofty::read_from_path(path).ok()?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())?;

    let picture = tag.pictures().first()?;
    let mime = picture.mime_type()?;
    let mime_str = mime.as_str().to_string();
    Some((picture.data().to_vec(), mime_str))
}
