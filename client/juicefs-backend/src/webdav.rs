use std::time::SystemTime;

use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::{header, Method, StatusCode, Url};

use crate::backend::{BackendError, JuiceFsBackend, Metadata, Result};

/// Talks to a `juicefs webdav` server (run as a localhost sidecar).
///
/// This is the spike backend: HTTP + XML, whole-file `PUT`. It exists to prove
/// the CfAPI/File Provider layer against something trivial. The production data
/// path is expected to move to a libjfs sidecar behind the same trait.
pub struct WebDavBackend {
    base: Url,
    client: reqwest::Client,
    auth: Option<(String, String)>,
}

impl WebDavBackend {
    /// `base_url` is the root of the WebDAV server, e.g. `http://127.0.0.1:9007/`.
    pub fn new(base_url: &str) -> Result<Self> {
        // Ensure the base ends in `/` so `Url::join` treats it as a directory.
        let normalized = if base_url.ends_with('/') {
            base_url.to_string()
        } else {
            format!("{base_url}/")
        };
        let base = Url::parse(&normalized)
            .map_err(|e| BackendError::Protocol(format!("invalid base url: {e}")))?;
        Ok(Self {
            base,
            client: reqwest::Client::new(),
            auth: None,
        })
    }

    pub fn with_basic_auth(mut self, user: impl Into<String>, pass: impl Into<String>) -> Self {
        self.auth = Some((user.into(), pass.into()));
        self
    }

    fn url_for(&self, path: &str) -> Result<Url> {
        let rel = path.trim_start_matches('/');
        self.base
            .join(rel)
            .map_err(|e| BackendError::Protocol(format!("bad path {path}: {e}")))
    }

    fn apply_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Some((u, p)) => rb.basic_auth(u, Some(p)),
            None => rb,
        }
    }

    async fn propfind(&self, path: &str, depth: &str) -> Result<String> {
        let url = self.url_for(path)?;
        let method = Method::from_bytes(b"PROPFIND").expect("valid method token");
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop>
<resourcetype/><getcontentlength/><getlastmodified/><getetag/>
</prop></propfind>"#;

        let rb = self
            .client
            .request(method, url)
            .header("Depth", depth)
            .header(header::CONTENT_TYPE, "application/xml")
            .body(body.to_string());

        let resp = self
            .apply_auth(rb)
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;

        match resp.status() {
            StatusCode::NOT_FOUND => Err(BackendError::NotFound(path.to_string())),
            StatusCode::UNAUTHORIZED => Err(BackendError::Unauthorized),
            s if s.is_success() => resp
                .text()
                .await
                .map_err(|e| BackendError::Transport(e.to_string())),
            s => Err(BackendError::Protocol(format!("PROPFIND {path} -> {s}"))),
        }
    }
}

#[async_trait]
impl JuiceFsBackend for WebDavBackend {
    async fn stat(&self, path: &str) -> Result<Metadata> {
        let xml = self.propfind(path, "0").await?;
        let entry = parse_multistatus(&xml)?
            .into_iter()
            .next()
            .ok_or_else(|| BackendError::NotFound(path.to_string()))?;
        Ok(raw_to_meta(entry))
    }

    async fn readdir(&self, path: &str) -> Result<Vec<Metadata>> {
        let xml = self.propfind(path, "1").await?;
        let entries = parse_multistatus(&xml)?;
        let self_path = self.url_for(path)?.path().to_string();
        Ok(into_dir_listing(entries, &self_path))
    }

    async fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let url = self.url_for(path)?;
        let end = offset + len - 1;
        let rb = self
            .client
            .get(url)
            .header(header::RANGE, format!("bytes={offset}-{end}"));

        let resp = self
            .apply_auth(rb)
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;

        match resp.status() {
            StatusCode::NOT_FOUND => return Err(BackendError::NotFound(path.to_string())),
            StatusCode::UNAUTHORIZED => return Err(BackendError::Unauthorized),
            // 206 Partial Content on success; 200 if the server ignored Range
            // (we then return the whole body — callers slice if needed).
            s if !s.is_success() => {
                return Err(BackendError::Protocol(format!("GET {path} -> {s}")))
            }
            _ => {}
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        Ok(bytes.to_vec())
    }

    async fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        let url = self.url_for(path)?;
        let rb = self.client.put(url).body(data.to_vec());
        let resp = self
            .apply_auth(rb)
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;

        match resp.status() {
            StatusCode::UNAUTHORIZED => Err(BackendError::Unauthorized),
            s if s.is_success() => Ok(()),
            s => Err(BackendError::Protocol(format!("PUT {path} -> {s}"))),
        }
    }
}

// ---- PROPFIND multistatus parsing -----------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct RawEntry {
    href: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    etag: Option<String>,
}

/// Strip an XML namespace prefix: `D:href` -> `href`.
fn local_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

/// Extract just the path portion of a WebDAV `href` (absolute URL or abs-path),
/// percent-decoded and without a trailing slash, for self-entry comparison.
fn href_path(href: &str) -> String {
    let decoded = percent_encoding::percent_decode_str(href)
        .decode_utf8_lossy()
        .into_owned();
    let path = match Url::parse(&decoded) {
        Ok(u) => u.path().to_string(),
        Err(_) => decoded,
    };
    path.trim_end_matches('/').to_string()
}

/// Last path segment of an `href`, percent-decoded — the entry's display name.
fn href_to_name(href: &str) -> String {
    let decoded = percent_encoding::percent_decode_str(href)
        .decode_utf8_lossy()
        .into_owned();
    decoded
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

fn raw_to_meta(e: RawEntry) -> Metadata {
    Metadata {
        name: href_to_name(&e.href),
        is_dir: e.is_dir,
        size: e.size,
        modified: e.modified,
        etag: e.etag,
    }
}

/// Drop the entry that refers to the queried directory itself (Depth: 1
/// includes it), then map the rest to public [`Metadata`].
fn into_dir_listing(entries: Vec<RawEntry>, self_path: &str) -> Vec<Metadata> {
    let self_norm = self_path.trim_end_matches('/');
    entries
        .into_iter()
        .filter(|e| href_path(&e.href) != self_norm)
        .map(raw_to_meta)
        .collect()
}

enum Field {
    Href,
    Size,
    Modified,
    Etag,
}

fn parse_multistatus(xml: &str) -> Result<Vec<RawEntry>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();

    let mut entries = Vec::new();
    let mut cur: Option<RawEntry> = None;
    let mut target: Option<Field> = None;
    let mut in_resourcetype = false;

    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| BackendError::Protocol(format!("xml: {e}")))?;
        match ev {
            Event::Eof => break,
            Event::Start(e) => match local_name(e.name().as_ref()).as_str() {
                "response" => {
                    cur = Some(RawEntry {
                        href: String::new(),
                        is_dir: false,
                        size: 0,
                        modified: None,
                        etag: None,
                    })
                }
                "href" => target = Some(Field::Href),
                "getcontentlength" => target = Some(Field::Size),
                "getlastmodified" => target = Some(Field::Modified),
                "getetag" => target = Some(Field::Etag),
                "resourcetype" => in_resourcetype = true,
                "collection" => {
                    if in_resourcetype {
                        if let Some(c) = cur.as_mut() {
                            c.is_dir = true;
                        }
                    }
                }
                _ => {}
            },
            Event::Empty(e) => {
                if in_resourcetype && local_name(e.name().as_ref()) == "collection" {
                    if let Some(c) = cur.as_mut() {
                        c.is_dir = true;
                    }
                }
            }
            Event::Text(t) => {
                if let (Some(field), Some(c)) = (&target, cur.as_mut()) {
                    let txt = t
                        .unescape()
                        .map_err(|e| BackendError::Protocol(format!("xml text: {e}")))?
                        .into_owned();
                    let txt = txt.trim();
                    match field {
                        Field::Href => c.href = txt.to_string(),
                        Field::Size => c.size = txt.parse().unwrap_or(0),
                        Field::Modified => c.modified = httpdate::parse_http_date(txt).ok(),
                        Field::Etag => c.etag = Some(txt.trim_matches('"').to_string()),
                    }
                }
            }
            Event::End(e) => match local_name(e.name().as_ref()).as_str() {
                "href" | "getcontentlength" | "getlastmodified" | "getetag" => target = None,
                "resourcetype" => in_resourcetype = false,
                "response" => {
                    if let Some(c) = cur.take() {
                        entries.push(c);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/Projects/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:getlastmodified>Mon, 08 Jun 2026 10:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/Projects/part%20one.stl</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>2048</D:getcontentlength>
        <D:getlastmodified>Mon, 08 Jun 2026 11:00:00 GMT</D:getlastmodified>
        <D:getetag>"abc123"</D:getetag>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    #[test]
    fn parses_dir_and_file() {
        let entries = parse_multistatus(SAMPLE).expect("parse");
        assert_eq!(entries.len(), 2);

        let dir = &entries[0];
        assert_eq!(dir.href, "/Projects/");
        assert!(dir.is_dir);

        let file = &entries[1];
        assert!(!file.is_dir);
        assert_eq!(file.size, 2048);
        assert_eq!(file.etag.as_deref(), Some("abc123"));
        assert!(file.modified.is_some());
    }

    #[test]
    fn readdir_filters_self_and_decodes_names() {
        let entries = parse_multistatus(SAMPLE).expect("parse");
        let listing = into_dir_listing(entries, "/Projects");

        assert_eq!(
            listing.len(),
            1,
            "the directory's own entry must be dropped"
        );
        assert_eq!(listing[0].name, "part one.stl"); // percent-decoded
        assert_eq!(listing[0].size, 2048);
    }

    #[test]
    fn href_path_handles_absolute_urls_and_paths() {
        assert_eq!(href_path("http://127.0.0.1:9007/Projects/"), "/Projects");
        assert_eq!(href_path("/Projects/"), "/Projects");
        assert_eq!(
            href_path("/Projects/part%20one.stl"),
            "/Projects/part one.stl"
        );
    }

    #[test]
    fn empty_resourcetype_is_a_file() {
        let xml = r#"<D:multistatus xmlns:D="DAV:"><D:response>
            <D:href>/a.txt</D:href>
            <D:propstat><D:prop>
              <D:resourcetype></D:resourcetype>
              <D:getcontentlength>5</D:getcontentlength>
            </D:prop></D:propstat>
        </D:response></D:multistatus>"#;
        let entries = parse_multistatus(xml).expect("parse");
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, 5);
    }
}
