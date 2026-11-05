use crate::map::Circle;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

pub(crate) const SCHEMA: &str = include_str!("../schema.sql");

pub(crate) fn parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub(crate) fn open(path: &Path) -> Result<Connection> {
    parent(path)?;
    let db = Connection::open(path).context("could not open SQLite database")?;
    db.busy_timeout(std::time::Duration::from_secs(60))?;
    db.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
    )?;
    db.execute_batch(SCHEMA)?;
    backfill(&db)?;
    Ok(db)
}

pub(crate) fn sync(
    db: &mut Connection,
    circles: &[Circle],
    event_id: &str,
    event_name: &str,
) -> Result<()> {
    ensure!(
        !event_id.is_empty()
            && event_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "event id must contain lowercase letters, digits or hyphens"
    );
    ensure!(!event_name.trim().is_empty(), "event name cannot be empty");
    let tx = db.transaction()?;
    for circle in circles {
        let mut metadata = serde_json::to_value(circle)?;
        metadata["event_id"] = json!(event_id);
        metadata["event_name"] = json!(event_name);
        tx.execute("INSERT INTO circles VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET name=excluded.name,metadata=excluded.metadata",
            params![circle.id,circle.name,metadata.to_string()])?;
        register(&tx, &circle.id, &metadata)?;
        tx.execute("DELETE FROM sources WHERE circle_id=?1", [&circle.id])?;
        for source in &circle.sources {
            tx.execute(
                "INSERT OR IGNORE INTO sources VALUES (?1,?2,?3)",
                params![circle.id, source.url, source.caption],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn list(db: &Connection, query: &str) -> Result<Vec<(String, String)>> {
    Ok(db
        .prepare("SELECT id,name FROM circles WHERE name LIKE ?1 ORDER BY name")?
        .query_map([format!("%{query}%")], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn sources(db: &Connection, id: &str, limit: usize) -> Result<Vec<String>> {
    Ok(db
        .prepare("SELECT url FROM sources WHERE circle_id=?1 ORDER BY url LIMIT ?2")?
        .query_map(params![id, i64::try_from(limit)?], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn has_circle(db: &Connection, id: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM circles WHERE id=?1)",
        [id],
        |r| r.get(0),
    )?)
}

pub(crate) fn cached(db: &Connection, key: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM extractions WHERE cache_key=?1)",
        [key],
        |r| r.get(0),
    )?)
}

pub(crate) fn associate(
    db: &Connection,
    id: &str,
    source: &str,
    image: &str,
    key: &str,
) -> Result<()> {
    // The association, current-page switch and item materialization commit together.
    let tx = db.unchecked_transaction()?;
    tx.execute("INSERT INTO results VALUES (?1,?2,?3,?4) ON CONFLICT(circle_id,source,image_source) DO UPDATE SET cache_key=excluded.cache_key",params![id,source,image,key])?;
    materialize(&tx, id, source, image, key)?;
    tx.commit()?;
    crate::dedupe::refresh_circle(db, id)?;
    Ok(())
}

fn stable_id(kind: &str, parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.len().to_string().as_bytes());
        hash.update([0]);
        hash.update(part.as_bytes());
    }
    format!("{kind}:{:x}", hash.finalize())
}

fn register(db: &Connection, id: &str, metadata: &Value) -> Result<()> {
    let Some(event_id) = metadata["event_id"].as_str().or_else(|| {
        // The only pre-event schema import in this project was CF23. Unknown
        // dates stay unassigned until explicitly synced with an event identifier.
        (metadata["event_date"] == "2026-10-31" && id.starts_with("cf.mikira.id:"))
            .then_some("cf23")
    }) else {
        return Ok(());
    };
    let name = metadata["name"].as_str().context("circle name missing")?;
    let date = metadata["event_date"]
        .as_str()
        .context("event date missing")?;
    let map = metadata["map_url"].as_str().context("map URL missing")?;
    let event_name = metadata["event_name"]
        .as_str()
        .unwrap_or(if event_id == "cf23" {
            "Comic Frontier 23"
        } else {
            event_id
        });
    if let Some(existing) = db
        .query_row(
            "SELECT event_id FROM event_circles WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        ensure!(
            existing == event_id,
            "map entry is already assigned to a different event"
        );
    }
    db.execute(
        "INSERT INTO events VALUES (?1,?2,?3,?4) ON CONFLICT(id) DO NOTHING",
        params![event_id, event_name, date, map],
    )?;
    let profile = stable_id("circle", &[id]);
    db.execute(
        "INSERT INTO circle_profiles VALUES (?1,?2) ON CONFLICT(id) DO NOTHING",
        params![profile, name],
    )?;
    let schedules = metadata
        .get("schedules")
        .cloned()
        .unwrap_or_else(|| json!([]));
    db.execute("INSERT INTO event_circles VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name,schedules_json=excluded.schedules_json",
        params![id,event_id,profile,name,schedules.to_string()])?;
    Ok(())
}

fn materialize(db: &Connection, circle: &str, source: &str, image: &str, key: &str) -> Result<()> {
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM event_circles WHERE id=?1)",
            [circle],
            |r| r.get::<_, bool>(0)
        )?,
        "circle is not assigned to an event; run sync --event <event-id>"
    );
    let (raw, time) = db.query_row(
        "SELECT catalog,created_at FROM extractions WHERE cache_key=?1",
        [key],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?;
    let catalog = crate::catalog::parse(&raw)?;
    let page = stable_id("page", &[circle, source, image, key]);
    db.execute("UPDATE catalog_pages SET is_current=0 WHERE event_circle_id=?1 AND source_url=?2 AND image_source=?3 AND id<>?4",
        params![circle,source,image,page])?;
    db.execute("INSERT INTO catalog_pages VALUES (?1,?2,?3,?4,?5,?6,1) ON CONFLICT(id) DO UPDATE SET is_current=1",params![page,circle,source,image,key,time])?;
    for (position, item) in catalog.items.iter().enumerate() {
        let id = stable_id("item", &[&page, &position.to_string()]);
        db.execute("INSERT INTO items (id,catalog_page_id,position,name,category,fandom,characters_json,price_text,price_amount,currency,variants_json,availability,evidence_text,confidence) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) ON CONFLICT(id) DO NOTHING",
            params![id,page,i64::try_from(position)?,item.name,item.category,item.fandom,serde_json::to_string(&item.characters)?,item.price_text,item.price_amount,item.currency,serde_json::to_string(&item.variants)?,item.availability,item.evidence_text,item.confidence])?;
    }
    Ok(())
}

fn backfill(db: &Connection) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    let mut statement = tx.prepare(
        "SELECT id,metadata FROM circles WHERE id NOT IN (SELECT id FROM event_circles)",
    )?;
    let rows = statement
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (id, metadata) in rows {
        register(&tx, &id, &serde_json::from_str(&metadata)?)?;
    }
    let mut statement = tx.prepare("SELECT r.circle_id,r.source,r.image_source,r.cache_key FROM results r JOIN event_circles ec ON ec.id=r.circle_id WHERE NOT EXISTS (SELECT 1 FROM catalog_pages p WHERE p.event_circle_id=r.circle_id AND p.source_url=r.source AND p.image_source=r.image_source AND p.extraction_key=r.cache_key)")?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (circle, source, image, key) in rows {
        materialize(&tx, &circle, &source, &image, &key)?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn link_profile(db: &Connection, participation: &str, profile: &str) -> Result<()> {
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM circle_profiles WHERE id=?1)",
            [profile],
            |r| r.get::<_, bool>(0)
        )?,
        "unknown circle profile"
    );
    let changed = db.execute(
        "UPDATE event_circles SET circle_profile_id=?1 WHERE id=?2",
        params![profile, participation],
    )?;
    ensure!(changed == 1, "unknown event participation");
    Ok(())
}

pub(crate) fn export(db: &Connection, output: &Path) -> Result<()> {
    let mut statement = db.prepare("SELECT id,metadata FROM circles ORDER BY id")?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut circles = Vec::new();
    for row in rows {
        let (id, metadata) = row?;
        let mut circle: Value = serde_json::from_str(&metadata)?;
        if let Some((event, profile)) = db
            .query_row(
                "SELECT event_id,circle_profile_id FROM event_circles WHERE id=?1",
                [&id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            circle["event_id"] = json!(event);
            circle["circle_profile_id"] = json!(profile);
        }
        let mut items_statement = db.prepare("SELECT json_object('id',id,'name',name,'category',category,'fandom',fandom,'characters',json(characters_json),'price_text',price_text,'price_amount',price_amount,'currency',currency,'variants',json(variants_json),'availability',availability,'evidence_text',evidence_text,'confidence',confidence,'review_status',review_status,'catalog_page_id',catalog_page_id,'source_url',source_url,'image_source',image_source,'extraction_key',extraction_key,'extracted_at',extracted_at) FROM event_items WHERE event_circle_id=?1 ORDER BY catalog_page_id,position")?;
        let item_rows = items_statement
            .query_map([&id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        circle["items"] = json!(
            item_rows
                .iter()
                .map(|s| serde_json::from_str::<Value>(s))
                .collect::<serde_json::Result<Vec<_>>>()?
        );
        let mut statement = db.prepare("SELECT r.source,r.image_source,e.model,e.created_at,e.catalog FROM results r JOIN extractions e USING(cache_key) WHERE r.circle_id=?1 ORDER BY r.source,r.image_source")?;
        let rows = statement.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut catalogs = Vec::new();
        for row in rows {
            let (source, image, model, time, catalog) = row?;
            catalogs.push(json!({"source":source,"image_source":image,"model":model,"extracted_at":time,"needs_review":true,"data":serde_json::from_str::<Value>(&catalog)?}));
        }
        circle["catalogs"] = json!(catalogs);
        circles.push(circle);
    }
    let count = circles.len();
    let events = db.prepare("SELECT id,name,start_date,map_url FROM events ORDER BY start_date,id")?.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"start_date":r.get::<_,String>(2)?,"map_url":r.get::<_,String>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    parent(output)?;
    fs::write(
        output,
        format!(
            "{}\n",
            serde_json::to_string_pretty(
                &json!({"schema_version":2,"events":events,"circles":circles})
            )?
        ),
    )?;
    println!("Exported {count} circles to {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circle(id: &str, date: &str) -> Circle {
        Circle {
            id: id.into(),
            map_index: 0,
            event_date: date.into(),
            name: "Returning Circle".into(),
            schedules: vec![json!({"day":0,"booth_codes":["A-01a"]})],
            socials: vec![],
            map_url: "https://cf.mikira.id/".into(),
            sources: vec![],
        }
    }

    fn extraction(db: &Connection, key: &str, price: i64) {
        let catalog = json!({"circle_name":"Returning Circle","booth_codes":[],"notes":[],"items":[{
            "name":"Sticker","category":"sticker","fandom":null,"characters":[],"price_text":format!("Rp {price}"),
            "price_amount":price,"currency":"IDR","variants":[],"availability":null,"evidence_text":"Sticker", "confidence":0.9}]});
        db.execute(
            "INSERT INTO extractions VALUES (?1,?1,'test:free',?2,'2026-01-01T00:00:00Z')",
            params![key, catalog.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn isolates_event_prices_preserves_history_reviews_and_identity_links() {
        let root = tempfile::tempdir().unwrap();
        let mut db = open(&root.path().join("test.sqlite")).unwrap();
        sync(
            &mut db,
            &[circle("map:cf23:0", "2026-10-31")],
            "cf23",
            "Comic Frontier 23",
        )
        .unwrap();
        sync(
            &mut db,
            &[circle("map:cf24:0", "2027-01-01")],
            "cf24",
            "Comic Frontier 24",
        )
        .unwrap();
        let profile: String = db
            .query_row(
                "SELECT circle_profile_id FROM event_circles WHERE id='map:cf23:0'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        link_profile(&db, "map:cf24:0", &profile).unwrap();
        sync(
            &mut db,
            &[circle("map:cf24:0", "2027-01-01")],
            "cf24",
            "Comic Frontier 24",
        )
        .unwrap();
        assert_eq!(
            db.query_row(
                "SELECT circle_profile_id FROM event_circles WHERE id='map:cf24:0'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            profile
        );
        extraction(&db, "first", 15000);
        associate(&db, "map:cf23:0", "post", "image", "first").unwrap();
        associate(&db, "map:cf24:0", "post", "image", "first").unwrap();
        db.execute("UPDATE items SET review_status='approved'", [])
            .unwrap();
        associate(&db, "map:cf23:0", "post", "image", "first").unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        extraction(&db, "second", 25000);
        associate(&db, "map:cf24:0", "post", "image", "second").unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            db.query_row(
                "SELECT price_amount FROM event_items WHERE event_id='cf23'",
                [],
                |r| r.get::<_, f64>(0)
            )
            .unwrap(),
            15000.0
        );
        assert_eq!(
            db.query_row(
                "SELECT price_amount FROM event_items WHERE event_id='cf24'",
                [],
                |r| r.get::<_, f64>(0)
            )
            .unwrap(),
            25000.0
        );
        assert_eq!(
            db.query_row(
                "SELECT review_status FROM event_items WHERE event_id='cf23'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "approved"
        );
        assert!(
            sync(
                &mut db,
                &[circle("map:cf23:0", "2026-10-31")],
                "cf24",
                "Wrong event"
            )
            .is_err()
        );
        let output = root.path().join("export.json");
        export(&db, &output).unwrap();
        let data: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert_eq!(data["schema_version"], 2);
        assert_eq!(data["events"].as_array().unwrap().len(), 2);
        assert_eq!(data["circles"][0]["items"][0]["price_amount"], 15000.0);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            0
        );
    }

    #[test]
    fn backfills_old_schema_and_rolls_back_invalid_extraction_association() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("legacy.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(SCHEMA.split("-- The four").next().unwrap())
            .unwrap();
        let circle = circle("cf.mikira.id:2026-10-31:0", "2026-10-31");
        db.execute(
            "INSERT INTO circles VALUES (?1,?2,?3)",
            params![
                circle.id,
                circle.name,
                serde_json::to_string(&circle).unwrap()
            ],
        )
        .unwrap();
        extraction(&db, "old", 15000);
        db.execute(
            "INSERT INTO results VALUES (?1,'post','image','old')",
            [&circle.id],
        )
        .unwrap();
        drop(db);
        let db = open(&path).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM event_items WHERE event_id='cf23'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        db.execute(
            "INSERT INTO extractions VALUES ('bad','bad','test:free','{}','now')",
            [],
        )
        .unwrap();
        assert!(associate(&db, &circle.id, "post", "image", "bad").is_err());
        assert_eq!(
            db.query_row("SELECT cache_key FROM results", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "old"
        );
        assert_eq!(
            db.query_row(
                "SELECT extraction_key FROM catalog_pages WHERE is_current=1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "old"
        );
        drop(db);
        let db = open(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
