CREATE TABLE IF NOT EXISTS circles (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, metadata TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sources (
    circle_id TEXT NOT NULL REFERENCES circles(id), url TEXT NOT NULL,
    caption TEXT NOT NULL DEFAULT '', PRIMARY KEY(circle_id, url)
);
CREATE TABLE IF NOT EXISTS extractions (
    cache_key TEXT PRIMARY KEY, image_sha256 TEXT NOT NULL,
    model TEXT NOT NULL, catalog TEXT NOT NULL, created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS results (
    circle_id TEXT NOT NULL REFERENCES circles(id), source TEXT NOT NULL,
    image_source TEXT NOT NULL, cache_key TEXT NOT NULL REFERENCES extractions(cache_key),
    PRIMARY KEY(circle_id, source, image_source)
);

-- The four tables above retain the original import/cache format for compatibility.
-- Profiles represent persistent identities; legacy circles represent map entries.
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    start_date TEXT NOT NULL,
    map_url TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS circle_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS event_circles (
    id TEXT PRIMARY KEY REFERENCES circles(id),
    event_id TEXT NOT NULL REFERENCES events(id),
    circle_profile_id TEXT NOT NULL REFERENCES circle_profiles(id),
    display_name TEXT NOT NULL,
    schedules_json TEXT NOT NULL CHECK (json_valid(schedules_json)),
    UNIQUE (event_id, circle_profile_id)
);
CREATE INDEX IF NOT EXISTS event_circles_profile ON event_circles(circle_profile_id);

CREATE TABLE IF NOT EXISTS catalog_pages (
    id TEXT PRIMARY KEY,
    event_circle_id TEXT NOT NULL REFERENCES event_circles(id),
    source_url TEXT NOT NULL,
    image_source TEXT NOT NULL,
    extraction_key TEXT NOT NULL REFERENCES extractions(cache_key),
    extracted_at TEXT NOT NULL,
    is_current INTEGER NOT NULL DEFAULT 1 CHECK (is_current IN (0, 1)),
    UNIQUE (event_circle_id, source_url, image_source, extraction_key)
);
CREATE UNIQUE INDEX IF NOT EXISTS catalog_pages_current
    ON catalog_pages(event_circle_id, source_url, image_source) WHERE is_current = 1;
CREATE INDEX IF NOT EXISTS catalog_pages_circle ON catalog_pages(event_circle_id);

CREATE TABLE IF NOT EXISTS items (
    id TEXT PRIMARY KEY,
    catalog_page_id TEXT NOT NULL REFERENCES catalog_pages(id),
    position INTEGER NOT NULL CHECK (position >= 0),
    name TEXT NOT NULL,
    category TEXT,
    fandom TEXT,
    characters_json TEXT NOT NULL CHECK (json_valid(characters_json)),
    price_text TEXT,
    price_amount REAL CHECK (price_amount IS NULL OR price_amount >= 0),
    currency TEXT,
    variants_json TEXT NOT NULL CHECK (json_valid(variants_json)),
    availability TEXT,
    evidence_text TEXT NOT NULL,
    confidence REAL NOT NULL CHECK (confidence BETWEEN 0 AND 1),
    review_status TEXT NOT NULL DEFAULT 'needs_review'
        CHECK (review_status IN ('needs_review', 'approved', 'rejected')),
    UNIQUE (catalog_page_id, position)
);
CREATE INDEX IF NOT EXISTS items_page ON items(catalog_page_id);
CREATE INDEX IF NOT EXISTS items_fandom ON items(fandom);

CREATE VIEW IF NOT EXISTS event_items AS
SELECT i.*, ec.event_id, ec.circle_profile_id, ec.id AS event_circle_id,
       ec.display_name AS circle_name, ec.schedules_json,
       p.source_url, p.image_source, p.extraction_key, p.extracted_at
FROM items i
JOIN catalog_pages p ON p.id = i.catalog_page_id
JOIN event_circles ec ON ec.id = p.event_circle_id
WHERE p.is_current = 1;

CREATE TABLE IF NOT EXISTS scrape_attempts (
    circle_id TEXT NOT NULL REFERENCES circles(id),
    source_url TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('processed', 'failed')),
    detail TEXT NOT NULL,
    attempted_at TEXT NOT NULL,
    PRIMARY KEY (circle_id, source_url)
);

CREATE TABLE IF NOT EXISTS image_fingerprints (
    image_sha256 TEXT PRIMARY KEY,
    pixel_sha256 TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    dhash TEXT NOT NULL,
    ahash TEXT NOT NULL,
    mean_luma INTEGER NOT NULL,
    algorithm TEXT NOT NULL DEFAULT 'dhash-ahash-v1'
);
CREATE INDEX IF NOT EXISTS image_fingerprints_pixels ON image_fingerprints(pixel_sha256);

CREATE TABLE IF NOT EXISTS catalog_page_duplicates (
    page_id TEXT NOT NULL REFERENCES catalog_pages(id),
    duplicate_of TEXT NOT NULL REFERENCES catalog_pages(id),
    match_kind TEXT NOT NULL CHECK (match_kind IN ('exact_image','similar_image')),
    score REAL NOT NULL CHECK (score BETWEEN 0 AND 1),
    decision TEXT NOT NULL CHECK (decision IN ('candidate','confirmed','rejected')),
    active INTEGER NOT NULL CHECK (active IN (0,1)),
    PRIMARY KEY (page_id, duplicate_of),
    CHECK (page_id <> duplicate_of)
);
CREATE TABLE IF NOT EXISTS item_duplicates (
    item_id TEXT NOT NULL REFERENCES items(id),
    duplicate_of TEXT NOT NULL REFERENCES items(id),
    match_kind TEXT NOT NULL CHECK (match_kind IN ('exact_image','same_item_fields','similar_item')),
    score REAL NOT NULL CHECK (score BETWEEN 0 AND 1),
    decision TEXT NOT NULL CHECK (decision IN ('candidate','confirmed','rejected')),
    active INTEGER NOT NULL CHECK (active IN (0,1)),
    PRIMARY KEY (item_id, duplicate_of),
    CHECK (item_id <> duplicate_of)
);

CREATE VIEW IF NOT EXISTS unique_event_items AS
SELECT e.* FROM event_items e
WHERE NOT EXISTS (
    SELECT 1 FROM item_duplicates d
    JOIN event_items target ON target.id=d.duplicate_of
    WHERE d.item_id=e.id AND d.active=1 AND d.decision='confirmed'
      AND target.event_circle_id=e.event_circle_id
);

-- Local upload ledger, never uploaded. Fingerprints include the destination.
CREATE TABLE IF NOT EXISTS d1_sync_state (
    destination TEXT NOT NULL,
    table_name TEXT NOT NULL,
    row_key TEXT NOT NULL,
    row_hash TEXT NOT NULL,
    PRIMARY KEY(destination,table_name,row_key)
);
CREATE TABLE IF NOT EXISTS scrape_run_state (
    event_id TEXT PRIMARY KEY REFERENCES events(id),
    status TEXT NOT NULL,
    detail TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    wake_at TEXT
);

CREATE TABLE IF NOT EXISTS source_coverage (
    circle_id TEXT NOT NULL REFERENCES circles(id),
    source_url TEXT NOT NULL,
    method TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('complete','partial','blocked')),
    slide_count INTEGER NOT NULL,
    manifest_path TEXT NOT NULL,
    detail TEXT,
    captured_at TEXT NOT NULL,
    PRIMARY KEY(circle_id,source_url)
);
