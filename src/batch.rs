use crate::{ExtractionOptions, d1::D1, ingest};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

fn candidates(
    db: &Connection,
    event: &str,
    limit: usize,
    retry_failed: bool,
) -> Result<Vec<(String, String, String)>> {
    // One catalog post per circle per batch. Public X photos are preferred over
    // Instagram previews; noncatalog announcements are not selected automatically.
    let sql = "WITH ranked AS (
        SELECT s.circle_id,s.url,ec.display_name,
          ROW_NUMBER() OVER (PARTITION BY s.circle_id ORDER BY CASE WHEN s.url LIKE 'https://x.com/%' THEN 0 ELSE 1 END,s.url) AS rank
        FROM sources s JOIN event_circles ec ON ec.id=s.circle_id
        LEFT JOIN scrape_attempts a ON a.circle_id=s.circle_id AND a.source_url=s.url
        LEFT JOIN source_coverage coverage ON coverage.circle_id=s.circle_id AND coverage.source_url=s.url
        WHERE ec.event_id=?1
          AND (lower(s.caption) LIKE '%catalog%' OR lower(s.caption) LIKE '%katalog%')
          AND ((NOT ?2 AND (a.status IS NULL OR
               (a.status='processed' AND s.url LIKE '%instagram.com/%' AND coverage.method IS NULL)))
               OR (?2 AND a.status='failed'))
        ) SELECT circle_id,url,display_name FROM ranked
          WHERE ?2 OR rank=1 ORDER BY display_name,url LIMIT ?3";
    let mut statement = db.prepare(sql)?;
    Ok(statement
        .query_map(params![event, retry_failed, i64::try_from(limit)?], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn run(
    db: &Connection,
    db_path: &Path,
    event: &str,
    limit: usize,
    max_images: usize,
    interval: f64,
    upload: bool,
    retry_failed: bool,
) -> Result<()> {
    let (_, failed) = run_batch(
        db,
        db_path,
        event,
        limit,
        max_images,
        interval,
        1,
        upload,
        retry_failed,
    )?;
    ensure!(
        failed == 0,
        "{failed} sources failed; successful pages are saved"
    );
    Ok(())
}

fn run_batch(
    db: &Connection,
    db_path: &Path,
    event: &str,
    limit: usize,
    max_images: usize,
    interval: f64,
    concurrency: usize,
    upload: bool,
    retry_failed: bool,
) -> Result<(usize, usize)> {
    let sources = candidates(db, event, limit, retry_failed)?;
    if sources.is_empty() {
        return Ok((0, 0));
    }
    println!(
        "Starting real {event} batch: {} circles, at most {max_images} images per post, concurrency {concurrency}",
        sources.len(),
    );
    let sources = Arc::new(sources);
    let next = AtomicUsize::new(0);
    let processed = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let signal = Mutex::new(None);
    let worker_count = concurrency.min(sources.len());
    let worker_results = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let sources = Arc::clone(&sources);
            let db_path = db_path.to_path_buf();
            let next = &next;
            let processed = &processed;
            let failed = &failed;
            let stop = &stop;
            let signal = &signal;
            handles.push(scope.spawn(move || -> Result<()> {
                let worker_db = Connection::open(&db_path)
                    .with_context(|| format!("worker could not open {}", db_path.display()))?;
                worker_db.busy_timeout(Duration::from_secs(60))?;
                worker_db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
                loop {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::AcqRel);
                    let Some((circle, source, name)) = sources.get(index) else {
                        break;
                    };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    println!("[{}/{}] {name}: {source}", index + 1, sources.len());
                    let options = ExtractionOptions {
                        circle: circle.clone(),
                        max_images,
                        interval,
                    };
                    let result = ingest(&worker_db, std::slice::from_ref(source), &options);
                    if let Err(error) = &result
                        && let Some(quota) = error.downcast_ref::<crate::quota::Stop>()
                    {
                        let kind = match quota {
                            crate::quota::Stop::DailyQuota => Signal::DailyQuota,
                            crate::quota::Stop::RateLimit => Signal::RateLimit,
                            crate::quota::Stop::Authentication => Signal::Authentication,
                        };
                        *signal.lock().expect("worker stop signal mutex poisoned") = Some(kind);
                        stop.store(true, Ordering::Release);
                        break;
                    }
                    let detail = match &result {
                        Ok(()) => "public catalog images processed".into(),
                        Err(error) => format!("{error:#}"),
                    };
                    worker_db.execute("INSERT INTO scrape_attempts VALUES (?1,?2,?3,?4,?5) ON CONFLICT(circle_id,source_url) DO UPDATE SET status=excluded.status,detail=excluded.detail,attempted_at=excluded.attempted_at",
                        params![circle,source,if result.is_ok(){"processed"}else{"failed"},detail,chrono::Utc::now().to_rfc3339()])?;
                    if result.is_ok() {
                        processed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                    let stop_signal = if detail.contains("OpenRouter HTTP 429") {
                        Some(Signal::RateLimit)
                    } else if ["OpenRouter HTTP 401", "OpenRouter HTTP 402", "OpenRouter HTTP 403"]
                        .iter().any(|code| detail.contains(code)) {
                        Some(Signal::Authentication)
                    } else {
                        None
                    };
                    if let Some(kind) = stop_signal {
                        *signal.lock().expect("worker stop signal mutex poisoned") = Some(kind);
                        stop.store(true, Ordering::Release);
                    }
                    let count: i64 = worker_db.query_row(
                        "SELECT COUNT(*) FROM event_items WHERE event_id=?1",
                        [event],
                        |row| row.get(0),
                    )?;
                    println!(
                        "Batch progress: {} processed, {} failed; {count} current {event} item observations",
                        processed.load(Ordering::Relaxed),
                        failed.load(Ordering::Relaxed),
                    );
                }
                Ok(())
            }));
        }
        let mut errors = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(_) => errors.push(anyhow::anyhow!("scrape worker panicked")),
            }
        }
        errors
    });
    if let Some(error) = worker_results.into_iter().next() {
        return Err(error);
    }
    let processed = processed.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    if let Some(signal) = *signal.lock().expect("worker stop signal mutex poisoned") {
        if upload {
            D1::from_env()?.push(db, true)?;
        }
        return Err(signal.into_error());
    }
    if upload {
        D1::from_env()?
            .push(db, true)
            .context("catalogs saved locally, but D1 upload failed")?;
    }
    println!("Batch finished: {processed} processed sources, {failed} failed sources");
    Ok((processed, failed))
}

#[derive(Clone, Copy)]
enum Signal {
    DailyQuota,
    RateLimit,
    Authentication,
}

impl Signal {
    fn into_error(self) -> anyhow::Error {
        let stop = match self {
            Self::DailyQuota => crate::quota::Stop::DailyQuota,
            Self::RateLimit => crate::quota::Stop::RateLimit,
            Self::Authentication => crate::quota::Stop::Authentication,
        };
        stop.into()
    }
}

fn state(
    db: &Connection,
    event: &str,
    status: &str,
    detail: &str,
    wake: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    db.execute("INSERT INTO scrape_run_state VALUES (?1,?2,?3,?4,?5) ON CONFLICT(event_id) DO UPDATE SET status=excluded.status,detail=excluded.detail,updated_at=excluded.updated_at,wake_at=excluded.wake_at",
        params![event,status,detail,chrono::Utc::now().to_rfc3339(),wake.map(|t|t.to_rfc3339())])?;
    println!("Run state: {status}: {detail}");
    Ok(())
}

fn wait_for_capacity(
    db: &Connection,
    event: &str,
    error: &anyhow::Error,
    upload: bool,
    wait: bool,
) -> Result<bool> {
    use crate::quota::Stop;
    let stop = error.downcast_ref::<Stop>();
    let wake = match stop {
        Some(Stop::DailyQuota) if wait => {
            let date = chrono::Utc::now()
                .date_naive()
                .succ_opt()
                .context("next UTC date unavailable")?;
            chrono::DateTime::from_naive_utc_and_offset(
                date.and_hms_opt(0, 1, 0)
                    .context("invalid UTC reset time")?,
                chrono::Utc,
            )
        }
        Some(Stop::RateLimit) if wait => chrono::Utc::now() + chrono::Duration::seconds(90),
        _ => {
            state(db, event, "blocked", &format!("{error:#}"), None)?;
            return Ok(false);
        }
    };
    state(
        db,
        event,
        "waiting_quota",
        &format!("{error:#}"),
        Some(wake),
    )?;
    if upload {
        D1::from_env()?.push(db, true)?;
    }
    while chrono::Utc::now() < wake {
        let seconds = (wake - chrono::Utc::now()).num_seconds().clamp(1, 60);
        std::thread::sleep(std::time::Duration::from_secs(u64::try_from(seconds)?));
    }
    Ok(true)
}

pub(crate) fn all(
    db: &Connection,
    db_path: &Path,
    event: &str,
    batch_size: usize,
    max_images: usize,
    interval: f64,
    concurrency: usize,
    upload: bool,
    wait: bool,
) -> Result<()> {
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE id=?1)",
            [event],
            |r| r.get::<_, bool>(0)
        )?,
        "unknown event"
    );
    crate::dedupe::backfill(db, event)?;
    state(
        db,
        event,
        "running",
        "processing all unattempted catalog sources",
        None,
    )?;
    if upload {
        D1::from_env()?.push(db, true)?;
    }
    loop {
        if candidates(db, event, batch_size, false)?.is_empty() {
            break;
        }
        match run_batch(
            db,
            db_path,
            event,
            batch_size,
            max_images,
            interval,
            concurrency,
            false,
            false,
        ) {
            Ok(_) => {
                state(
                    db,
                    event,
                    "running",
                    "batch checkpoint saved; continuing queue",
                    None,
                )?;
                if upload {
                    D1::from_env()?.push(db, true)?;
                }
            }
            Err(error) => {
                if !wait_for_capacity(db, event, &error, upload, wait)? {
                    return Err(error);
                }
                state(db, event, "running", "resuming after quota cooldown", None)?;
            }
        }
    }
    // Retry the failures once, not forever. Snapshot the count so permanently
    // unavailable posts cannot keep an all-batches job running indefinitely.
    let failed:i64=db.query_row("SELECT COUNT(*) FROM scrape_attempts a JOIN event_circles ec ON ec.id=a.circle_id WHERE ec.event_id=?1 AND a.status='failed'",[event],|r|r.get(0))?;
    if failed > 0 {
        state(
            db,
            event,
            "retrying",
            "one final pass over failed sources",
            None,
        )?;
        // One pass is bounded by the initial failure count; each selected source
        // is tried only once by requesting the snapshot in a single batch.
        loop {
            match run_batch(
                db,
                db_path,
                event,
                usize::try_from(failed)?,
                max_images,
                interval,
                concurrency,
                false,
                true,
            ) {
                Ok(_) => break,
                Err(error) => {
                    if !wait_for_capacity(db, event, &error, upload, wait)? {
                        return Err(error);
                    }
                }
            }
        }
    }
    let failed:i64=db.query_row("SELECT COUNT(*) FROM scrape_attempts a JOIN event_circles ec ON ec.id=a.circle_id WHERE ec.event_id=?1 AND a.status='failed'",[event],|r|r.get(0))?;
    state(
        db,
        event,
        if failed == 0 {
            "completed"
        } else {
            "completed_with_failures"
        },
        &format!("catalog queue exhausted; {failed} sources unavailable or failed"),
        None,
    )?;
    if upload {
        D1::from_env()?.push(db, true)?;
    }
    crate::storage::export(db, std::path::Path::new("data/catalogs.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resumes_unrecorded_partial_sources_and_retries_only_failures() {
        let root = tempfile::tempdir().unwrap();
        let db = crate::storage::open(&root.path().join("test.sqlite")).unwrap();
        db.execute_batch(
            "INSERT INTO events VALUES ('cf23','CF23','2026-10-31','https://example.org');
            INSERT INTO extractions VALUES ('cached','digest','test:free','{}','now');",
        )
        .unwrap();
        for id in ["processed", "failed", "interrupted"] {
            db.execute("INSERT INTO circles VALUES (?1,?1,'{}')", [id])
                .unwrap();
            db.execute("INSERT INTO circle_profiles VALUES (?1,?1)", [id])
                .unwrap();
            db.execute(
                "INSERT INTO event_circles VALUES (?1,'cf23',?1,?1,'[]')",
                [id],
            )
            .unwrap();
            db.execute(
                "INSERT INTO sources VALUES (?1,?2,'CF23 catalog')",
                params![id, format!("https://x.com/{id}/status/123")],
            )
            .unwrap();
        }
        for status in ["processed", "failed"] {
            db.execute(
                "INSERT INTO scrape_attempts VALUES (?1,?2,?1,'result','now')",
                params![status, format!("https://x.com/{status}/status/123")],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO sources VALUES ('failed','https://www.instagram.com/p/ALTERNATE/','CF23 catalog')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO scrape_attempts VALUES ('failed','https://www.instagram.com/p/ALTERNATE/','failed','result','now')",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO results VALUES ('interrupted','https://x.com/interrupted/status/123','image','cached')",[]).unwrap();
        let regular = candidates(&db, "cf23", 25, false).unwrap();
        assert_eq!(regular.len(), 1);
        assert_eq!(regular[0].0, "interrupted");
        let retry = candidates(&db, "cf23", 25, true).unwrap();
        assert_eq!(retry.len(), 2);
        assert_eq!(retry[0].0, "failed");
        assert_eq!(retry[1].0, "failed");
        assert!(candidates(&db, "cf24", 25, false).unwrap().is_empty());
        db.execute_batch("INSERT INTO circles VALUES ('preview','Preview','{}');
            INSERT INTO circle_profiles VALUES ('preview','Preview');
            INSERT INTO event_circles VALUES ('preview','cf23','preview','Preview','[]');
            INSERT INTO sources VALUES ('preview','https://www.instagram.com/p/ABC/','catalog');
            INSERT INTO scrape_attempts VALUES ('preview','https://www.instagram.com/p/ABC/','processed','old HTML preview','now');").unwrap();
        assert_eq!(candidates(&db, "cf23", 25, false).unwrap().len(), 2);
        db.execute_batch("INSERT INTO source_coverage VALUES ('preview','https://www.instagram.com/p/ABC/','playwright','complete',5,'manifest','done','now');").unwrap();
        assert_eq!(candidates(&db, "cf23", 25, false).unwrap().len(), 1);
    }
}
