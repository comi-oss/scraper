use crate::{
    net::{Http, read_limited},
    storage,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) struct D1 {
    http: Http,
    endpoint: String,
    token: String,
}

impl D1 {
    pub(crate) fn from_env() -> Result<Self> {
        let account = env("CLOUDFLARE_ACCOUNT_ID")?;
        let database = env("CLOUDFLARE_DATABASE_ID")?;
        ensure!(
            account.len() == 32 && account.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid Cloudflare account id"
        );
        ensure!(
            database.len() == 36 && database.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
            "invalid D1 database id"
        );
        Ok(Self {
            http: Http::new()?,
            endpoint: format!(
                "https://api.cloudflare.com/client/v4/accounts/{account}/d1/database/{database}/query"
            ),
            token: env("CLOUDFLARE_API_TOKEN")?,
        })
    }

    fn query(&self, body: &Value) -> Result<Value> {
        let response = self
            .http
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .map_err(|_| anyhow::anyhow!("D1 request failed"))?;
        let status = response.status().as_u16();
        let value: Value =
            serde_json::from_slice(&read_limited(response)?).context("invalid D1 response")?;
        let codes: Vec<_> = value["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["code"].as_u64())
            .collect();
        ensure!(
            (200..300).contains(&status) && value["success"] == true,
            "D1 HTTP {status}, error codes {codes:?}; check token D1 permissions, account and database id"
        );
        let results = value["result"]
            .as_array()
            .context("D1 query results missing")?;
        ensure!(
            !results.is_empty() && results.iter().all(|r| r["success"] == true),
            "a D1 SQL statement failed"
        );
        Ok(value)
    }

    pub(crate) fn check(&self) -> Result<()> {
        let response = self.query(&json!({"sql":"SELECT 1 AS connected","params":[]}))?;
        ensure!(
            response["result"][0]["results"][0]["connected"] == 1,
            "unexpected D1 connection-check response"
        );
        Ok(())
    }

    pub(crate) fn push(&self, db: &Connection, catalogs_only: bool) -> Result<()> {
        self.check()?;
        self.query(&json!({"sql":storage::SCHEMA,"params":[]}))?;
        let destination = format!("{:x}", Sha256::digest(&self.endpoint));
        // Parents precede children. Upserts allow safe retries after partial uploads.
        for (table, columns, conflict) in [
            ("circles", vec!["id", "name", "metadata"], vec!["id"]),
            (
                "sources",
                vec!["circle_id", "url", "caption"],
                vec!["circle_id", "url"],
            ),
            (
                "extractions",
                vec![
                    "cache_key",
                    "image_sha256",
                    "model",
                    "catalog",
                    "created_at",
                ],
                vec!["cache_key"],
            ),
            (
                "results",
                vec!["circle_id", "source", "image_source", "cache_key"],
                vec!["circle_id", "source", "image_source"],
            ),
            (
                "events",
                vec!["id", "name", "start_date", "map_url"],
                vec!["id"],
            ),
            ("circle_profiles", vec!["id", "name"], vec!["id"]),
            (
                "event_circles",
                vec![
                    "id",
                    "event_id",
                    "circle_profile_id",
                    "display_name",
                    "schedules_json",
                ],
                vec!["id"],
            ),
            (
                "catalog_pages",
                vec![
                    "id",
                    "event_circle_id",
                    "source_url",
                    "image_source",
                    "extraction_key",
                    "extracted_at",
                    "is_current",
                ],
                vec!["id"],
            ),
            (
                "items",
                vec![
                    "id",
                    "catalog_page_id",
                    "position",
                    "name",
                    "category",
                    "fandom",
                    "characters_json",
                    "price_text",
                    "price_amount",
                    "currency",
                    "variants_json",
                    "availability",
                    "evidence_text",
                    "confidence",
                    "review_status",
                ],
                vec!["id"],
            ),
            (
                "scrape_attempts",
                vec![
                    "circle_id",
                    "source_url",
                    "status",
                    "detail",
                    "attempted_at",
                ],
                vec!["circle_id", "source_url"],
            ),
            (
                "image_fingerprints",
                vec![
                    "image_sha256",
                    "pixel_sha256",
                    "width",
                    "height",
                    "dhash",
                    "ahash",
                    "mean_luma",
                    "algorithm",
                ],
                vec!["image_sha256"],
            ),
            (
                "catalog_page_duplicates",
                vec![
                    "page_id",
                    "duplicate_of",
                    "match_kind",
                    "score",
                    "decision",
                    "active",
                ],
                vec!["page_id", "duplicate_of"],
            ),
            (
                "item_duplicates",
                vec![
                    "item_id",
                    "duplicate_of",
                    "match_kind",
                    "score",
                    "decision",
                    "active",
                ],
                vec!["item_id", "duplicate_of"],
            ),
            (
                "source_coverage",
                vec![
                    "circle_id",
                    "source_url",
                    "method",
                    "status",
                    "slide_count",
                    "manifest_path",
                    "detail",
                    "captured_at",
                ],
                vec!["circle_id", "source_url"],
            ),
            (
                "scrape_run_state",
                vec!["event_id", "status", "detail", "updated_at", "wake_at"],
                vec!["event_id"],
            ),
        ] {
            if catalogs_only
                && !matches!(
                    table,
                    "extractions"
                        | "results"
                        | "catalog_pages"
                        | "items"
                        | "scrape_attempts"
                        | "image_fingerprints"
                        | "catalog_page_duplicates"
                        | "item_duplicates"
                        | "source_coverage"
                        | "scrape_run_state"
                )
            {
                continue;
            }
            let updates = columns
                .iter()
                // Review choices made in D1 Studio survive subsequent uploads.
                .filter(|c| !conflict.contains(c) && !matches!(**c, "review_status" | "decision"))
                .map(|c| format!("{c}=excluded.{c}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT({}) DO UPDATE SET {updates}",
                columns.join(","),
                vec!["?"; columns.len()].join(","),
                conflict.join(",")
            );
            // Table/column names are fixed constants; row values are bound parameters.
            // Retire existing page versions before inserting new current versions.
            let order = if table == "catalog_pages" {
                " ORDER BY is_current ASC"
            } else {
                ""
            };
            let mut statement =
                db.prepare(&format!("SELECT {} FROM {table}{order}", columns.join(",")))?;
            let rows = statement.query_map([], |row| {
                (0..columns.len())
                    .map(|i| match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => Ok(Value::Null),
                        rusqlite::types::ValueRef::Integer(n) => Ok(json!(n)),
                        rusqlite::types::ValueRef::Real(n) => Ok(json!(n)),
                        rusqlite::types::ValueRef::Text(s) => Ok(json!(String::from_utf8_lossy(s))),
                        rusqlite::types::ValueRef::Blob(_) => Err(rusqlite::Error::InvalidQuery),
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?;
            let mut batch = Vec::new();
            let mut ledger = Vec::new();
            let mut count = 0;
            let mut uploaded = 0;
            for row in rows {
                let values = row?;
                count += 1;
                let keys = conflict
                    .iter()
                    .map(|key| {
                        let index = columns
                            .iter()
                            .position(|c| c == key)
                            .context("upload key column missing")?;
                        Ok(values[index].clone())
                    })
                    .collect::<Result<Vec<_>>>()?;
                let row_key = serde_json::to_string(&keys)?;
                let row_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&values)?));
                let previous=db.query_row("SELECT row_hash FROM d1_sync_state WHERE destination=?1 AND table_name=?2 AND row_key=?3",params![destination,table,row_key],|r|r.get::<_,String>(0)).optional()?;
                if previous.as_deref() == Some(&row_hash) {
                    continue;
                }
                batch.push(json!({"sql":sql,"params":values}));
                ledger.push((row_key, row_hash));
                uploaded += 1;
                if batch.len() == 50 {
                    self.flush(db, &destination, table, &batch, &ledger)?;
                    batch.clear();
                    ledger.clear();
                }
            }
            if !batch.is_empty() {
                self.flush(db, &destination, table, &batch, &ledger)?;
            }
            let result = self.query(
                &json!({"sql":format!("SELECT COUNT(*) AS count FROM {table}"),"params":[]}),
            )?;
            let remote = result["result"][0]["results"][0]["count"]
                .as_u64()
                .context("D1 count missing")?;
            ensure!(remote >= count, "D1 verification failed for {table}");
            println!(
                "Uploaded {uploaded} changed {table} rows ({count} local); D1 contains {remote}"
            );
        }
        Ok(())
    }

    fn flush(
        &self,
        db: &Connection,
        destination: &str,
        table: &str,
        batch: &[Value],
        ledger: &[(String, String)],
    ) -> Result<()> {
        self.query(&json!({"batch":batch}))?;
        let tx = db.unchecked_transaction()?;
        for (key, hash) in ledger {
            tx.execute("INSERT INTO d1_sync_state VALUES (?1,?2,?3,?4) ON CONFLICT(destination,table_name,row_key) DO UPDATE SET row_hash=excluded.row_hash",params![destination,table,key,hash])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn reset_upload_state(&self, db: &Connection) -> Result<()> {
        let destination = format!("{:x}", Sha256::digest(&self.endpoint));
        db.execute(
            "DELETE FROM d1_sync_state WHERE destination=?1",
            [destination],
        )?;
        Ok(())
    }
}

fn env(name: &str) -> Result<String> {
    let value =
        std::env::var(name).with_context(|| format!("set {name} in .env or your environment"))?;
    ensure!(!value.trim().is_empty(), "{name} is empty");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_typed_items_page_history_and_preserves_remote_reviews() {
        use std::sync::{Arc, Mutex};
        let remote = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        remote
            .lock()
            .unwrap()
            .execute_batch("PRAGMA foreign_keys=ON")
            .unwrap();
        let remote_http = Arc::clone(&remote);
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .expect_at_least(1)
            .with_body_from_request(move |request| {
                let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                let queries = body
                    .get("batch")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_else(|| vec![body]);
                let db = remote_http.lock().unwrap();
                let mut results = Vec::new();
                for query in queries {
                    let sql = query["sql"].as_str().unwrap();
                    let params: Vec<_> = query["params"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|v| match v {
                            Value::Null => rusqlite::types::Value::Null,
                            Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                            Value::Number(n) if n.is_i64() => {
                                rusqlite::types::Value::Integer(n.as_i64().unwrap())
                            }
                            Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap()),
                            _ => panic!("unsupported D1 parameter"),
                        })
                        .collect();
                    let mut rows = Vec::new();
                    if sql.trim_start().starts_with("CREATE") {
                        db.execute_batch(sql).unwrap();
                    } else if sql.starts_with("SELECT") {
                        let mut statement = db.prepare(sql).unwrap();
                        let names: Vec<_> = statement
                            .column_names()
                            .iter()
                            .map(|s| s.to_string())
                            .collect();
                        let mut result = statement
                            .query(rusqlite::params_from_iter(&params))
                            .unwrap();
                        while let Some(row) = result.next().unwrap() {
                            let mut object = serde_json::Map::new();
                            for (index, name) in names.iter().enumerate() {
                                // All verification SELECTs return a single integer column.
                                object
                                    .insert(name.clone(), json!(row.get::<_, i64>(index).unwrap()));
                            }
                            rows.push(Value::Object(object));
                        }
                    } else {
                        db.execute(sql, rusqlite::params_from_iter(&params))
                            .unwrap();
                    }
                    results.push(json!({"success":true,"results":rows}));
                }
                json!({"success":true,"result":results})
                    .to_string()
                    .into_bytes()
            })
            .create();
        let root = tempfile::tempdir().unwrap();
        let db = storage::open(&root.path().join("local.sqlite")).unwrap();
        db.execute_batch(
            "INSERT INTO circles VALUES ('one','Test','{}');
            INSERT INTO events VALUES ('cf23','CF23','2026-10-31','https://example.org');
            INSERT INTO circle_profiles VALUES ('profile','Test');
            INSERT INTO event_circles VALUES ('one','cf23','profile','Test','[]');",
        )
        .unwrap();
        let mut catalog = json!({"circle_name":"Test","booth_codes":[],"notes":[],"items":[{
            "name":"Sticker","category":null,"fandom":null,"characters":[],"price_text":"Rp 15k",
            "price_amount":15000,"currency":"IDR","variants":[],"availability":null,"evidence_text":"Rp 15k","confidence":0.9}]});
        db.execute(
            "INSERT INTO extractions VALUES ('v1','digest','test:free',?1,'now')",
            [catalog.to_string()],
        )
        .unwrap();
        storage::associate(&db, "one", "source", "image", "v1").unwrap();
        let d1 = D1 {
            http: Http::new().unwrap(),
            endpoint: server.url(),
            token: "test".into(),
        };
        d1.push(&db, false).unwrap();
        remote
            .lock()
            .unwrap()
            .execute("UPDATE items SET review_status='approved'", [])
            .unwrap();
        catalog["items"][0]["price_amount"] = json!(25000);
        db.execute(
            "INSERT INTO extractions VALUES ('v2','digest2','test:free',?1,'later')",
            [catalog.to_string()],
        )
        .unwrap();
        storage::associate(&db, "one", "source", "image", "v2").unwrap();
        d1.push(&db, true).unwrap();
        let remote = remote.lock().unwrap();
        assert_eq!(
            remote
                .query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            remote
                .query_row("SELECT COUNT(*) FROM event_items", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            remote
                .query_row("SELECT price_amount FROM event_items", [], |r| r
                    .get::<_, f64>(0))
                .unwrap(),
            25000.0
        );
        assert_eq!(
            remote
                .query_row("SELECT category FROM event_items", [], |r| r
                    .get::<_, Option<String>>(0))
                .unwrap(),
            None
        );
        assert_eq!(
            remote
                .query_row(
                    "SELECT review_status FROM items WHERE price_amount=15000",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "approved"
        );
        mock.assert();
    }

    #[test]
    fn checks_access_and_redacts_errors() {
        let mut server = mockito::Server::new();
        let ok = server
            .mock("POST", "/")
            .match_header("authorization", "Bearer secret")
            .match_body(mockito::Matcher::PartialJson(
                json!({"sql":"SELECT 1 AS connected","params":[]}),
            ))
            .with_status(200)
            .with_body(
                json!({"success":true,"result":[{"success":true,"results":[{"connected":1}]}]})
                    .to_string(),
            )
            .expect(1)
            .create();
        let d1 = D1 {
            http: Http::new().unwrap(),
            endpoint: server.url(),
            token: "secret".into(),
        };
        d1.check().unwrap();
        ok.assert();
        ok.remove();
        let denied = server
            .mock("POST", "/")
            .with_status(403)
            .with_body(
                json!({"success":false,"errors":[{"code":10000,"message":"secret"}]}).to_string(),
            )
            .create();
        let error = d1.check().unwrap_err().to_string();
        assert!(error.contains("403"));
        assert!(!error.contains("secret"));
        denied.assert();
    }
}
