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

pub(crate) fn x_photos(http: &Http, id: &str) -> Result<Vec<String>> {
    let mut endpoint = Url::parse("https://cdn.syndication.twimg.com/tweet-result")?;
    endpoint
        .query_pairs_mut()
        .append_pair("id", id)
        .append_pair("lang", "en")
        .append_pair("token", "0");
    let post: Value = serde_json::from_slice(&http.fetch(endpoint.as_str())?.bytes)
        .context("invalid public X embed response")?;
    photo_urls(&post, id)
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
}
