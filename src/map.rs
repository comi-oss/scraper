use crate::net::{Http, selector};
use anyhow::{Context, Result, ensure};
use reqwest::Url;
use scraper::Html;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Source {
    pub(crate) url: String,
    pub(crate) caption: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Circle {
    pub(crate) id: String,
    pub(crate) map_index: usize,
    pub(crate) event_date: String,
    pub(crate) name: String,
    pub(crate) schedules: Vec<Value>,
    pub(crate) socials: Vec<Value>,
    pub(crate) map_url: String,
    pub(crate) sources: Vec<Source>,
}

pub(crate) fn load(http: &Http, url: &str) -> Result<Vec<Circle>> {
    let document = http.fetch(url)?;
    let html = Html::parse_document(std::str::from_utf8(&document.bytes)?);
    let config = html
        .select(&selector("script#cfg")?)
        .next()
        .context("map configuration not found")?;
    let config: Value = serde_json::from_str(&config.inner_html())?;
    let mut docs = Vec::new();
    for key in ["data", "enrich", "details"] {
        let path = config[key].as_str().context("map asset URL missing")?;
        docs.push(serde_json::from_slice::<Value>(
            &http.fetch(document.url.join(path)?.as_str())?.bytes,
        )?);
    }
    decode(&docs[0], &docs[1], &docs[2], &document.url)
}

pub(crate) fn decode(
    data: &Value,
    enrich: &Value,
    details: &Value,
    url: &Url,
) -> Result<Vec<Circle>> {
    let version = data["v"].as_u64().context("map version missing")?;
    ensure!(
        matches!(version, 3 | 4),
        "unsupported map version {version}"
    );
    let booths: Vec<_> = data["b"]
        .as_str()
        .context("booth list missing")?
        .split_whitespace()
        .collect();
    let date = data["date"].as_str().context("event date missing")?;
    let mut cursor = 0i64;
    let mut circles = Vec::new();
    for (index, row) in data["c"]
        .as_array()
        .context("circle list missing")?
        .iter()
        .enumerate()
    {
        let index_key = index.to_string();
        let info = &enrich[&index_key];
        let mut schedules = Vec::new();
        for allocation in row[2].as_array().context("circle schedules missing")? {
            let allocation = allocation.as_array().context("invalid schedule")?;
            ensure!(
                !allocation.is_empty() && allocation.len() % 2 == 1,
                "invalid booth allocation"
            );
            let mut codes = Vec::new();
            for pair in allocation[1..].chunks_exact(2) {
                let delta = pair[0].as_i64().context("invalid booth offset")?;
                cursor = if version == 4 {
                    cursor.checked_add(delta).context("booth offset overflow")?
                } else {
                    delta
                };
                let start = usize::try_from(cursor).context("negative booth offset")?;
                let count = usize::try_from(pair[1].as_u64().context("invalid booth count")?)?;
                let end = start.checked_add(count).context("booth range overflow")?;
                ensure!(count > 0 && end <= booths.len(), "invalid booth range");
                codes.extend_from_slice(&booths[start..end]);
            }
            schedules.push(json!({"day": allocation[0], "booth_codes": codes}));
        }
        let socials = info["s"].as_array().cloned().unwrap_or_default();
        let accounts: Vec<_> = socials.iter().filter_map(Value::as_str).collect();
        let mut sources = Vec::new();
        if let Some(posts) = details[&index_key]["p"].as_array() {
            for post in posts {
                let post = post.as_array().context("invalid catalog post")?;
                match post.as_slice() {
                    [link, caption] => sources.push(Source {
                        url: link.as_str().context("invalid post URL")?.into(),
                        caption: caption.as_str().context("invalid caption")?.into(),
                    }),
                    [account, id, caption] => {
                        let account = if let Some(index) = account.as_u64() {
                            usize::try_from(index)
                                .ok()
                                .and_then(|i| accounts.get(i).copied())
                        } else {
                            account.as_str()
                        };
                        if let Some(account) = account {
                            sources.push(Source {
                                url: format!(
                                    "https://x.com/{account}/status/{}",
                                    id.as_str().context("invalid post id")?
                                ),
                                caption: caption.as_str().context("invalid caption")?.into(),
                            });
                        }
                    }
                    _ => anyhow::bail!("invalid catalog post format"),
                }
            }
        }
        let host = &url[url::Position::BeforeHost..url::Position::AfterPort];
        circles.push(Circle {
            id: format!("{host}:{date}:{index}"),
            map_index: index,
            event_date: date.into(),
            name: row[0].as_str().context("circle name missing")?.into(),
            schedules,
            socials,
            map_url: url.to_string(),
            sources,
        });
    }
    Ok(circles)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_cumulative_offsets_and_x_accounts() {
        let data = json!({"v":4,"date":"2026-10-31","b":"A-01a A-01b A-02a A-02b","c":[["One",-1,[[0,1,1]]],["Two",-1,[[1,1,2]]]]});
        let circles = decode(
            &data,
            &json!({"1":{"s":["artist"]}}),
            &json!({"1":{"p":[[0,"123","Catalog"]]}}),
            &Url::parse("https://cf.mikira.id/").unwrap(),
        )
        .unwrap();
        assert_eq!(
            circles[1].schedules[0]["booth_codes"],
            json!(["A-02a", "A-02b"])
        );
        assert_eq!(circles[1].sources[0].url, "https://x.com/artist/status/123");
        assert_eq!(circles[0].id, "cf.mikira.id:2026-10-31:0");
        let mut invalid = data;
        invalid["c"][0][2][0][1] = json!(-1);
        assert!(
            decode(
                &invalid,
                &json!({}),
                &json!({}),
                &Url::parse("https://example.org").unwrap()
            )
            .is_err()
        );
    }
}
