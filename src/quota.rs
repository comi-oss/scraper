use crate::net::{Http, read_limited};
use anyhow::{Result, ensure};
use serde_json::Value;

#[derive(Debug)]
pub(crate) enum Stop {
    DailyQuota,
    RateLimit,
    Authentication,
}
impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::DailyQuota => "free daily OpenRouter quota exhausted",
            Self::RateLimit => "temporary OpenRouter rate limit",
            Self::Authentication => "OpenRouter authentication/credit configuration failed",
        })
    }
}
impl std::error::Error for Stop {}

pub(crate) fn remaining(http: &Http, key: &str) -> Result<Option<u64>> {
    let response = http
        .client
        .get("https://openrouter.ai/api/v1/key")
        .bearer_auth(key)
        .send()
        .map_err(|_| anyhow::anyhow!("could not check free-model quota"))?;
    if matches!(response.status().as_u16(), 401..=403) {
        return Err(Stop::Authentication.into());
    }
    ensure!(
        response.status().is_success(),
        "quota check HTTP {}",
        response.status().as_u16()
    );
    let data: Value = serde_json::from_slice(&read_limited(response)?)?;
    Ok(data["data"]["free_model_daily_requests"]["remaining"].as_u64())
}
