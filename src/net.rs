use anyhow::{Context, Result, ensure};
use reqwest::{Url, blocking::Client};
use scraper::{Html, Selector};
use std::{collections::HashSet, io::Read, time::Duration};

pub(crate) const MAX_BYTES: u64 = 20 * 1024 * 1024;

pub(crate) struct Http {
    pub(crate) client: Client,
}

pub(crate) struct Document {
    pub(crate) bytes: Vec<u8>,
    pub(crate) content_type: String,
    pub(crate) url: Url,
}

impl Http {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(180))
                .user_agent("ComiCatalogScraper/0.2")
                .build()?,
        })
    }

    pub(crate) fn fetch(&self, url: &str) -> Result<Document> {
        let url = Url::parse(url).context("invalid source URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "only HTTP(S) URLs are supported"
        );
        let response = self
            .client
            .get(url)
            .send()
            .map_err(|_| anyhow::anyhow!("source request failed"))?;
        ensure!(
            response.status().is_success(),
            "source HTTP {}",
            response.status().as_u16()
        );
        let url = response.url().clone();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = read_limited(response)?;
        Ok(Document {
            bytes,
            content_type,
            url,
        })
    }
}

pub(crate) fn read_limited(reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("could not read source")?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "source exceeds 20 MiB limit"
    );
    Ok(bytes)
}

pub(crate) fn selector(raw: &str) -> Result<Selector> {
    Selector::parse(raw).map_err(|_| anyhow::anyhow!("invalid built-in HTML selector"))
}

pub(crate) fn images(html: &str, base: &Url) -> Result<Vec<String>> {
    let page = Html::parse_document(html);
    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    for element in page.select(&selector("meta, img")?) {
        let e = element.value();
        let src = if e.name() == "img" {
            e.attr("src")
        } else {
            match e.attr("property").or_else(|| e.attr("name")) {
                Some("og:image" | "og:image:url" | "twitter:image") => e.attr("content"),
                _ => None,
            }
        };
        if let Some(url) = src.and_then(|s| base.join(s).ok())
            && matches!(url.scheme(), "http" | "https")
            && seen.insert(url.to_string())
        {
            urls.push(url.to_string());
        }
    }
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_public_images_in_order_without_duplicates() {
        let html = r#"<meta property="og:image" content="/a.jpg"><img src="/a.jpg"><img src="b.png"><img src="data:x">"#;
        assert_eq!(
            images(html, &Url::parse("https://example.org/post/").unwrap()).unwrap(),
            [
                "https://example.org/a.jpg",
                "https://example.org/post/b.png"
            ]
        );
    }
    #[test]
    fn rejects_oversized_stream() {
        assert!(read_limited(std::io::repeat(0).take(MAX_BYTES + 1)).is_err());
    }
}
