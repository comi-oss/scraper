//! Public post-media discovery. No session cookies or authenticated endpoints.
use crate::net::Http;
use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde_json::Value;
use std::collections::HashSet;

pub(crate) fn x_status(source: &str) -> Option<String> {
    let url = Url::parse(source).ok()?;
    if !matches!(
        url.host_str()?,
        "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com"
    ) {
        return None;
    }
    let parts: Vec<_> = url.path_segments()?.collect();
    let index = parts.iter().position(|s| *s == "status")?;
    let id = *parts.get(index + 1)?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_owned())
}

pub(crate) fn x_photos(http: &Http, source: &str, id: &str) -> Result<Vec<String>> {
    let mut endpoint = Url::parse("https://cdn.syndication.twimg.com/tweet-result")?;
    endpoint
        .query_pairs_mut()
        .append_pair("id", id)
        .append_pair("lang", "en")
        .append_pair("token", "0");
    let embed_result = (|| {
        let post: Value = serde_json::from_slice(&fetch_with_retry(http, endpoint.as_str())?.bytes)
            .context("invalid public X embed response")?;
        photo_urls(&post, id)
    })();
    match embed_result {
        Ok(photos) => Ok(photos),
        Err(embed_error) => {
            // X's public syndication endpoint can omit attachments for posts
            // that its public status page still renders with media.
            let document = fetch_with_retry(http, source).with_context(|| {
                format!("syndication failed ({embed_error:#}); public X page fetch failed")
            })?;
            let photos = page_photo_urls(&document.bytes, &document.url, id)
                .with_context(|| format!("public X syndication failed ({embed_error:#})"))?;
            println!("X public page fallback passed: {} photo(s)", photos.len());
            Ok(photos)
        }
    }
}

fn page_photo_urls(body: &[u8], final_url: &Url, id: &str) -> Result<Vec<String>> {
    ensure!(
        final_url.host_str().is_some_and(|host| matches!(
            host,
            "x.com" | "www.x.com" | "twitter.com" | "www.twitter.com"
        )),
        "X status page redirected to another site"
    );
    ensure!(
        x_status(final_url.as_str()).as_deref() == Some(id),
        "X status page redirected away from requested post"
    );
    let html = String::from_utf8_lossy(body);
    let mut seen = HashSet::new();
    let photos = crate::net::images(&html, final_url)?
        .into_iter()
        .filter_map(|raw| {
            let url = Url::parse(&raw).ok()?;
            (url.scheme() == "https"
                && url.host_str() == Some("pbs.twimg.com")
                && url.path().starts_with("/media/")
                && url.query_pairs().any(|(key, value)| {
                    key == "name" && matches!(value.as_ref(), "large" | "orig" | "medium")
                }))
            .then_some(raw)
        })
        .filter(|url| seen.insert(url.clone()))
        .collect::<Vec<_>>();
    ensure!(!photos.is_empty(), "public X status page has no photos");
    Ok(photos)
}

/// X's public syndication and media hosts occasionally return transient errors.
/// Retry those reads before the caller records an image/source failure.
pub(crate) fn fetch_with_retry(http: &Http, url: &str) -> Result<crate::net::Document> {
    let mut last_error = None;
    for attempt in 0..3 {
        match http.fetch(url) {
            Ok(document) => return Ok(document),
            Err(error) => last_error = Some(error),
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_secs(1 << attempt));
        }
    }
    Err(last_error.expect("three fetch attempts"))
}

fn photo_urls(post: &Value, id: &str) -> Result<Vec<String>> {
    ensure!(
        post["id_str"].as_str() == Some(id),
        "public X post unavailable or missing matching identity"
    );
    let mut seen = HashSet::new();
    let mut images = Vec::new();
    for photo in post["photos"].as_array().into_iter().flatten() {
        if let Some(raw) = photo["url"].as_str() {
            let url = Url::parse(raw)?;
            ensure!(
                url.scheme() == "https"
                    && url.host_str() == Some("pbs.twimg.com")
                    && url.path().starts_with("/media/"),
                "unexpected X photo URL"
            );
            if seen.insert(raw.to_owned()) {
                images.push(raw.to_owned());
            }
        }
    }
    ensure!(!images.is_empty(), "public X post has no photos");
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn discovers_only_this_posts_photos_not_avatar_or_quote_images() {
        assert_eq!(
            x_status("https://x.com/artist/status/123/photo/1").as_deref(),
            Some("123")
        );
        assert!(x_status("https://not-x.com/artist/status/123").is_none());
        let post = json!({"id_str":"123","user":{"profile_image_url_https":"https://pbs.twimg.com/profile_images/avatar.jpg"},
            "photos":[{"url":"https://pbs.twimg.com/media/catalog.jpg"},{"url":"https://pbs.twimg.com/media/catalog.jpg"}],
            "quoted_tweet":{"photos":[{"url":"https://pbs.twimg.com/media/unrelated.jpg"}]}});
        assert_eq!(
            photo_urls(&post, "123").unwrap(),
            ["https://pbs.twimg.com/media/catalog.jpg"]
        );
        assert!(photo_urls(&post, "456").is_err());
    }

    #[test]
    fn status_page_fallback_uses_media_images_and_ignores_avatars() {
        let page = Url::parse("https://x.com/artist/status/123").unwrap();
        let html = br#"<meta property="og:image" content="https://pbs.twimg.com/profile_images/avatar.jpg"><img src="https://pbs.twimg.com/profile_images/avatar.jpg"><img src="https://pbs.twimg.com/media/placeholder.jpg?format=jpg&name=240x240"><img src="https://pbs.twimg.com/media/catalog-1.jpg?format=jpg&name=large"><img src="https://pbs.twimg.com/media/catalog-2.jpg?format=jpg&name=large">"#;
        assert_eq!(
            page_photo_urls(html, &page, "123").unwrap(),
            [
                "https://pbs.twimg.com/media/catalog-1.jpg?format=jpg&name=large",
                "https://pbs.twimg.com/media/catalog-2.jpg?format=jpg&name=large"
            ]
        );
        assert!(page_photo_urls(html, &page, "456").is_err());
    }
}
