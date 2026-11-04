//! Exact cache reuse and conservative, reviewable duplicate observations.
use crate::catalog::Item;
use anyhow::{Context, Result};
use image::{DynamicImage, imageops::FilterType};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};

#[derive(Debug)]
struct Fingerprint {
    pixels: String,
    width: u32,
    height: u32,
    dhash: u64,
    ahash: u64,
    luma: u8,
}

fn fingerprint(image: &DynamicImage) -> Fingerprint {
    let rgb = image.to_rgb8();
    let mut hash = Sha256::new();
    hash.update(image.width().to_le_bytes());
    hash.update(image.height().to_le_bytes());
    hash.update(rgb.as_raw());
    let gray = image.resize_exact(9, 8, FilterType::Triangle).to_luma8();
    let mut dhash = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            dhash = (dhash << 1) | u64::from(gray.get_pixel(x, y)[0] > gray.get_pixel(x + 1, y)[0]);
        }
    }
    let gray = image.resize_exact(8, 8, FilterType::Triangle).to_luma8();
    let mean = gray.as_raw().iter().map(|v| u32::from(*v)).sum::<u32>() / 64;
    let mut ahash = 0u64;
    for pixel in gray.as_raw() {
        ahash = (ahash << 1) | u64::from(u32::from(*pixel) > mean);
    }
    Fingerprint {
        pixels: format!("{:x}", hash.finalize()),
        width: image.width(),
        height: image.height(),
        dhash,
        ahash,
        luma: u8::try_from(mean).unwrap_or(255),
    }
}

/// Index decoded pixels, and reuse an existing extraction only for exact pixels.
pub(crate) fn index_image(
    db: &Connection,
    digest: &str,
    jpeg: &[u8],
    model: &str,
) -> Result<Option<String>> {
    let f = fingerprint(&image::load_from_memory(jpeg)?);
    db.execute("INSERT INTO image_fingerprints (image_sha256,pixel_sha256,width,height,dhash,ahash,mean_luma) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(image_sha256) DO NOTHING",
        params![digest,f.pixels,f.width,f.height,format!("{:016x}",f.dhash),format!("{:016x}",f.ahash),f.luma])?;
    Ok(db.query_row("SELECT e.cache_key FROM extractions e JOIN image_fingerprints f ON f.image_sha256=e.image_sha256 WHERE f.pixel_sha256=?1 AND e.model=?2 AND e.cache_key IN (SELECT extraction_key FROM catalog_pages) ORDER BY e.created_at LIMIT 1",
        params![f.pixels,model],|r|r.get(0)).optional()?)
}

fn normalized(raw: &str) -> String {
    raw.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}
fn array_key(values: &[String]) -> Vec<String> {
    let mut v: Vec<_> = values.iter().map(|s| normalized(s)).collect();
    v.sort();
    v
}
fn optional_key(raw: &Option<String>) -> Option<String> {
    raw.as_deref().map(normalized)
}

fn item_similarity(a: &Item, b: &Item) -> Option<(&'static str, f64)> {
    // Price, fandom, character and variant differences always preserve separate observations.
    if a.price_amount != b.price_amount
        || optional_key(&a.price_text) != optional_key(&b.price_text)
        || optional_key(&a.currency) != optional_key(&b.currency)
        || optional_key(&a.fandom) != optional_key(&b.fandom)
        || optional_key(&a.category) != optional_key(&b.category)
        || array_key(&a.characters) != array_key(&b.characters)
        || array_key(&a.variants) != array_key(&b.variants)
    {
        return None;
    }
    let an = normalized(&a.name);
    let bn = normalized(&b.name);
    if an.is_empty() || bn.is_empty() {
        return None;
    }
    if an == bn {
        return Some(("same_item_fields", 1.0));
    }
    let at: BTreeSet<_> = an.split_whitespace().collect();
    let bt: BTreeSet<_> = bn.split_whitespace().collect();
    let codes = |tokens: &BTreeSet<&str>| {
        tokens
            .iter()
            .filter(|s| s.chars().any(|c| c.is_ascii_digit()))
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>()
    };
    if codes(&at) != codes(&bt) {
        return None;
    }
    let score = at.intersection(&bt).count() as f64 / at.union(&bt).count() as f64;
    (score >= 0.85).then_some(("similar_item", score))
}

fn visually_similar(a: &Fingerprint, b: &Fingerprint) -> Option<f64> {
    let aspect_a = f64::from(a.width) / f64::from(a.height.max(1));
    let aspect_b = f64::from(b.width) / f64::from(b.height.max(1));
    let dh = (a.dhash ^ b.dhash).count_ones();
    let ah = (a.ahash ^ b.ahash).count_ones();
    if (aspect_a / aspect_b - 1.0).abs() > 0.01 || a.luma.abs_diff(b.luma) > 10 || dh > 4 || ah > 4
    {
        return None;
    }
    Some(1.0 - f64::from(dh + ah) / 128.0)
}

struct Page {
    id: String,
    key: String,
    fingerprint: Option<Fingerprint>,
    items: Vec<(String, usize, Item)>,
}

pub(crate) fn refresh_circle(db: &Connection, circle: &str) -> Result<()> {
    let mut statement=db.prepare("SELECT p.id,p.extraction_key,f.pixel_sha256,f.width,f.height,f.dhash,f.ahash,f.mean_luma FROM catalog_pages p JOIN extractions e ON e.cache_key=p.extraction_key LEFT JOIN image_fingerprints f ON f.image_sha256=e.image_sha256 WHERE p.event_circle_id=?1 AND p.is_current=1 ORDER BY p.rowid")?;
    let rows = statement
        .query_map([circle], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<u32>>(3)?,
                r.get::<_, Option<u32>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<u8>>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut pages = Vec::new();
    for (id, key, pixels, width, height, dh, ah, luma) in rows {
        let fingerprint = match (pixels, width, height, dh, ah, luma) {
            (Some(pixels), Some(width), Some(height), Some(dh), Some(ah), Some(luma)) => {
                Some(Fingerprint {
                    pixels,
                    width,
                    height,
                    dhash: u64::from_str_radix(&dh, 16)?,
                    ahash: u64::from_str_radix(&ah, 16)?,
                    luma,
                })
            }
            _ => None,
        };
        let raw: String = db.query_row(
            "SELECT catalog FROM extractions WHERE cache_key=?1",
            [&key],
            |r| r.get(0),
        )?;
        let catalog = crate::catalog::parse(&raw)?;
        let ids = db
            .prepare("SELECT id,position FROM items WHERE catalog_page_id=?1 ORDER BY position")?
            .query_map([&id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let items = ids
            .into_iter()
            .map(|(id, pos)| {
                let item = catalog
                    .items
                    .get(pos)
                    .context("item position outside extraction")?
                    .clone();
                Ok((id, pos, item))
            })
            .collect::<Result<Vec<_>>>()?;
        pages.push(Page {
            id,
            key,
            fingerprint,
            items,
        });
    }
    let tx = db.unchecked_transaction()?;
    tx.execute("UPDATE catalog_page_duplicates SET active=0 WHERE page_id IN (SELECT id FROM catalog_pages WHERE event_circle_id=?1)",[circle])?;
    tx.execute("UPDATE item_duplicates SET active=0 WHERE item_id IN (SELECT i.id FROM items i JOIN catalog_pages p ON p.id=i.catalog_page_id WHERE p.event_circle_id=?1)",[circle])?;
    for (index, page) in pages.iter().enumerate() {
        // Earlier current pages are canonical: generated links cannot form cycles.
        for other in &pages[..index] {
            let exact = page.key == other.key
                || matches!((&page.fingerprint,&other.fingerprint),(Some(a),Some(b)) if a.pixels==b.pixels);
            let similar = match (&page.fingerprint, &other.fingerprint) {
                (Some(a), Some(b)) => visually_similar(a, b),
                _ => None,
            };
            if exact || similar.is_some() {
                tx.execute("INSERT INTO catalog_page_duplicates VALUES (?1,?2,?3,?4,?5,1) ON CONFLICT(page_id,duplicate_of) DO UPDATE SET active=1,score=excluded.score",
                    params![page.id,other.id,if exact{"exact_image"}else{"similar_image"},if exact{1.0}else{similar.unwrap_or_default()},if exact{"confirmed"}else{"candidate"}])?;
            }
            for (id, pos, item) in &page.items {
                for (target, target_pos, other_item) in &other.items {
                    let similarity = if exact && pos == target_pos {
                        Some(("exact_image", 1.0))
                    } else {
                        item_similarity(item, other_item)
                    };
                    if let Some((kind, score)) = similarity {
                        tx.execute("INSERT INTO item_duplicates VALUES (?1,?2,?3,?4,?5,1) ON CONFLICT(item_id,duplicate_of) DO UPDATE SET active=1,score=excluded.score",
                            params![id,target,kind,score,if kind=="exact_image"{"confirmed"}else{"candidate"}])?;
                    }
                }
            }
            if exact {
                break;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn backfill(db: &Connection, event: &str) -> Result<()> {
    let rows=db.prepare("SELECT DISTINCT e.image_sha256,e.model FROM extractions e JOIN catalog_pages p ON p.extraction_key=e.cache_key JOIN event_circles ec ON ec.id=p.event_circle_id WHERE ec.event_id=?1")?.query_map([event],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (digest, model) in rows {
        let path = Path::new("data/images").join(format!("{digest}.jpg"));
        if path.exists() {
            index_image(db, &digest, &std::fs::read(path)?, &model)?;
        }
    }
    let circles = db
        .prepare("SELECT id FROM event_circles WHERE event_id=?1")?
        .query_map([event], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for circle in circles {
        refresh_circle(db, &circle)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(name: &str, price: f64) -> Item {
        serde_json::from_value(serde_json::json!({"name":name,"category":"print","fandom":null,"characters":[],"price_text":"35k","price_amount":price,"currency":null,"variants":[],"availability":null,"evidence_text":"35k","confidence":0.9})).unwrap()
    }
    #[test]
    fn prices_and_variant_codes_are_never_collapsed() {
        assert!(
            item_similarity(
                &item("Genshin print A1", 35000.0),
                &item("Genshin print A1", 45000.0)
            )
            .is_none()
        );
        assert!(
            item_similarity(
                &item("Genshin print A1", 35000.0),
                &item("Genshin print A2", 35000.0)
            )
            .is_none()
        );
        assert_eq!(
            item_similarity(
                &item("Genshin PRINT A1", 35000.0),
                &item("genshin print a1", 35000.0)
            )
            .unwrap()
            .0,
            "same_item_fields"
        );
    }
    #[test]
    fn resized_images_are_only_similarity_candidates() {
        let mut rgb = image::RgbImage::new(120, 160);
        for (x, y, p) in rgb.enumerate_pixels_mut() {
            *p = image::Rgb([u8::try_from(x).unwrap(), u8::try_from(y).unwrap(), 100]);
        }
        let image = DynamicImage::ImageRgb8(rgb);
        let resized = image.resize_exact(240, 320, FilterType::Triangle);
        let a = fingerprint(&image);
        let b = fingerprint(&resized);
        assert_ne!(a.pixels, b.pixels);
        assert!(visually_similar(&a, &b).is_some());
        let c = fingerprint(&image.rotate90());
        assert!(visually_similar(&a, &c).is_none());
    }
}
