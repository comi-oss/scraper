# Comi catalog scraper

Rust CLI to extract booth merchandise using OpenRouter's
`nvidia/nemotron-3.5-super-vl:free`. Imports booth metadata and catalog-post links
from [cf.mikira.id](https://cf.mikira.id/), the map loaded by
[Comi](https://github.com/comi-oss/comi).

## Setup and usage

Requires Rust 1.88+, Node.js 22+, and Playwright Chromium for Instagram captures.
SQLite is bundled; Python is not needed.

```sh
cargo build --locked
npm ci
npx playwright install chromium
cp .env.example .env
# Set OPENROUTER_API_KEY in .env. Edit existing .env files rather than replacing them.
cargo run -- sync --event cf23 --event-name 'Comic Frontier 23'
cargo run -- list ichigowarano
cargo run -- ingest ./catalog.jpg --circle 'cf.mikira.id:2026-10-31:0'
cargo run -- crawl --circle 'cf.mikira.id:2026-10-31:0' --limit 1
cargo run -- export --output data/catalogs.json
```

Run a resumable real catalog batch (one post per selected circle):

```sh
cargo run -- scrape --event cf23 --limit 25 --max-images 20 --upload
# Retry failed sources in a later batch:
cargo run -- scrape --event cf23 --limit 25 --max-images 20 --upload --retry-failed
# Process the remaining queue and wait for daily free quota resets:
cargo run -- scrape-all --event cf23 --batch-size 25 --max-images 20 --concurrency 3 --upload --wait-for-quota
```

The batch prioritizes public X photo posts with catalog captions. `scrape_attempts`
records processed/failed sources; processed means the requested image budget was
handled, not that a circle's entire catalog/thread was retrieved. Without
`--retry-failed`, subsequent batches skip recorded failures. `scrape-all`
defaults to one worker; `--concurrency 3` processes up to three circles at once.
Results upload at each batch checkpoint with `--upload`. D1 must already contain
the event/booth metadata.
Quota/authentication errors stop the batch after saving progress.

Before extracting X images, the scraper checks the public syndication response
matches the requested post ID and contains that post's photos. If syndication
omits photos, it checks the matching public status page and accepts only full-size
X media images. Instagram preflight checks that the public HTML stays on the same
post; when preview metadata is present, it must point to Instagram's media CDN.
Missing preview metadata still proceeds to Playwright, which checks the visible
post and captures its carousel.

Use the exact circle ID returned by `list`. Ingest also accepts direct HTTP(S)
image URLs and public HTML catalog pages. Place `--db path/to/db.sqlite` before
the subcommand to choose another database. `--max-images 20` bounds images per
source, `--interval 4` spaces uncached VLM requests, and `crawl --limit` bounds
source links. Build a standalone binary using `cargo build --release --locked`.

The Python implementation has been replaced. Existing SQLite data is retained;
JSON exports now use schema version 2 with events and per-circle items.
Old Python virtual environments can be removed when
no longer needed. Shell variables override .env; secrets and data are gitignored.

## Cloudflare D1

Create an API token with D1 write permission scoped to your account (dashboard:
**Account → D1 → Edit**). An R2-only token or S3 keys are insufficient. Add to .env:

```dotenv
CLOUDFLARE_ACCOUNT_ID=your_account_id
CLOUDFLARE_DATABASE_ID=your_database_uuid
CLOUDFLARE_API_TOKEN=your_api_token
```

Account ID is on the account overview; database UUID is on the D1 page. The example
file contains the existing comi database UUID.

```sh
cargo run -- d1-check
cargo run -- d1-push
# After booth metadata is already uploaded, save D1 writes with:
cargo run -- d1-push --catalogs-only
```

`d1-check` is read-only. `d1-push` creates missing tables and uploads local records
using the [D1 REST API](https://developers.cloudflare.com/api/resources/d1/subresources/database/methods/query/),
bound parameters, and batches of at most 50 statements. Parents upload before
children. Matching primary keys update; unrelated remote rows are retained. Each
batch commits separately; rerun failed uploads to finish. Counts are checked after
each table; this is not checksum verification. A local upload ledger sends only new
or changed rows. Use `--force` to reset it and re-upload after deliberately rebuilding
the remote database. Item reviews and duplicate decisions made in D1 are preserved.

This is one-way upload, not bidirectional replication. Local SQLite remains the
working cache. Metadata, links/captions, extraction JSON, and associations upload;
images and credentials stay local. Comi does not yet consume this data. Add a
Worker with a D1 binding and read API for app access. Never embed an administrative
Cloudflare token in the Tauri webview.

## Sources and limitations

Map configuration supplies current data/enrich/details URLs. V4 booth deltas and
compact X links are decoded; circle IDs include the event date and upstream index.
The map links mostly to X/Instagram catalog posts. X photo discovery uses its public
syndication/embed endpoint, checking the requested post ID and selecting only that
post's attached photos (excluding avatars and quoted posts). This endpoint is
best-effort and may change; if it omits photos, the scraper verifies the exact
status page and uses only full-size `pbs.twimg.com/media` images as a fallback.
Protected, unavailable, or media-less posts remain failures.
Instagram `/p/` links now use Playwright automatically. The helper closes normal
public UI prompts, rewinds to the first slide, clicks Next until the end, and saves
2x screenshots after each image decodes and remains stable. It waits up to 60
seconds per slide by default; set `COMI_INSTAGRAM_IMAGE_TIMEOUT_MS` (5–180 seconds)
to change the limit. It does not remove DOM login walls, solve
challenges, load browser cookies, or log in. Manifests and PNGs live in ignored
`data/instagram-captures/`. The `source_coverage` table records `complete`, `partial`,
or `blocked`. Slide limits and failed navigation never count as complete. Previously
processed Instagram preview-only links are re-queued for the browser method.

```sh
npm run capture:instagram -- 'https://www.instagram.com/p/DdOt7-oExHQ/' --max-slides 20
```

Other public HTML discovery reads
og:image, twitter:image, and img src. Login walls, JavaScript, carousels, and expired
URLs can block discovery or expose only previews/unrelated artwork. There is no
authenticated social connector or access-control bypass. Download accessible pages
and ingest each image for reliable coverage. PDFs are unsupported.
Captions remain metadata, not product evidence.

JPEG, PNG, WebP, and GIF inputs are capped at 20 MiB and decoded with allocation
limits. Metadata orientation is applied; images become JPEGs at up to 2400 pixels
per side. Split dense pages to preserve small text. Validated JSON includes item
names, categories, fandoms, characters, variants, availability, printed/numeric
prices, evidence, and confidence. Unknowns are nullable. Confidence is model-reported;
exports carry needs_review=true. Map booths remain separate from model-read codes.

Caching includes image bytes, model, and pipeline version. Rust/Pillow JPEG encoding
differs: Python results remain accessible, but first Rust ingestion may need a fresh
VLM call. Successful pages remain saved if later pages fail; failures exit nonzero.
Normalized images are archived in ignored `data/images/` using their content hash.
Exact decoded pixels can reuse an existing VLM extraction. Perceptual image hashes
and matching item fields create review candidates, without silently merging edited
images, variants, prices, circles, or editions. Deterministic same-image reposts
within a circle/event are confirmed duplicates. No observations are deleted.
Use `unique_event_items` to omit confirmed duplicates, or `event_items` to inspect
all observations. Run `cargo run -- dedupe --event cf23` to backfill old images.
Perceptual matching is conservative and can miss crops, letterboxing or rotations.
Only :free models are accepted with zero provider price caps and no paid fallback.
Transient 429/502/503/504 responses retry at most three times. See
[OpenRouter limits](https://openrouter.ai/docs/api/reference/limits).
The [free endpoint](https://openrouter.ai/nvidia/nemotron-3.5-super-vl:free) has NVIDIA
logging/data-use terms and excludes confidential/personal data. Send appropriate
merchandise-only pages.

## Checks

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
npm test
```

Tests cover booth decoding, image discovery, input limits, schema validation,
retries/truncation, caching/export, and D1 access errors. Local HTTP mocks consume
no API quota.

## Run all batches in the background

Build `cargo build --release --locked`, then install the sample
[user service](ops/comi-cf23-scrape.service) into `~/.config/systemd/user/`. Adjust
workspace and Node paths for other installations, then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now comi-cf23-scrape.service
journalctl --user -u comi-cf23-scrape.service -f
```

The worker reads the existing ignored `.env`. `flock` prevents duplicate copies of
this service. The sample runs three circles concurrently. Each new VLM call checks
free quota. Daily exhaustion pauses until
00:01 UTC; temporary rate limits get a shorter cooldown. There is no paid fallback.
Partial sources resume from cached images. Each batch checkpoints changed rows to
D1. At queue exhaustion, failed sources get one final pass before the job records
completion with an explicit failure count. Public posts may remain inaccessible.

Inspect `scrape_run_state`, `scrape_attempts`, `source_coverage`, `item_duplicates`,
and `catalog_page_duplicates` in D1 Studio. The machine must stay on and connected.
The user service resumes after login; running through logout without login depends
on the host's user-manager configuration. Stop it with
`systemctl --user stop comi-cf23-scrape.service`.

The [database design](docs/database.md) documents event editions, persistent circle
identities, booth participation, VLM item rows, provenance, revision history, and
SQL queries for browsing CF23 merchandise in D1 Studio.
