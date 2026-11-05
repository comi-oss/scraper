import { chromium } from 'playwright';
import { mkdir, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const configuredImageWait = Number(process.env.COMI_INSTAGRAM_IMAGE_TIMEOUT_MS ?? 60000);
const IMAGE_WAIT_TIMEOUT_MS = Number.isInteger(configuredImageWait) && configuredImageWait >= 5000 && configuredImageWait <= 180000
  ? configuredImageWait : 60000;

export function normalizePostUrl(raw) {
  const url = new URL(raw);
  if (url.protocol !== 'https:' || !['instagram.com', 'www.instagram.com'].includes(url.hostname)) {
    throw new Error('Expected a public HTTPS Instagram post URL');
  }
  const match = url.pathname.match(/^\/(?:[^/]+\/)?p\/([A-Za-z0-9_-]+)\/?$/);
  if (!match) throw new Error('Expected an Instagram /p/ post URL');
  return `https://www.instagram.com/p/${match[1]}/`;
}

export async function dismissOverlays(page) {
  let count = 0;
  for (let attempt = 0; attempt < 4; attempt++) {
    const body = await page.locator('body').innerText();
    if (!/See this post in the app|Never miss a post|Sign up to see photos|Log in to continue/i.test(body)) break;
    const close = page.getByRole('button', { name: /^(close|not now)$/i }).first();
    if (!(await close.isVisible().catch(() => false))) break;
    await close.click({ timeout: 3000 });
    count++;
    await page.waitForTimeout(400);
  }
  const cookies = page.getByRole('button', { name: /^(decline optional cookies|only allow essential cookies)$/i }).first();
  if (await cookies.isVisible().catch(() => false)) await cookies.click({ timeout: 3000 });
  return count;
}

async function visibleImage(page, scoped = true) {
  const found = await page.evaluate((scoped) => {
    const root = scoped && document.querySelector('[data-comi-carousel]') || document.querySelector('article') || document.querySelector('main') || document;
    const candidates = [...root.querySelectorAll('img')].flatMap(img => {
      if (!img.complete || img.naturalWidth < 250 || img.naturalHeight < 250 || /profile picture|avatar/i.test(img.alt)) return [];
      const r = img.getBoundingClientRect();
      let left = Math.max(0, r.left), top = Math.max(0, r.top), right = Math.min(innerWidth, r.right), bottom = Math.min(innerHeight, r.bottom);
      for (let p = img.parentElement; p; p = p.parentElement) {
        const css = getComputedStyle(p), box = p.getBoundingClientRect();
        if (/(hidden|clip|scroll|auto)/.test(css.overflowX)) { left = Math.max(left, box.left); right = Math.min(right, box.right); }
        if (/(hidden|clip|scroll|auto)/.test(css.overflowY)) { top = Math.max(top, box.top); bottom = Math.min(bottom, box.bottom); }
      }
      const width = Math.max(0, right-left), height = Math.max(0, bottom-top);
      if (width < 200 || height < 200 || width*height < r.width*r.height*0.9) return [];
      // Instagram's controls and carousel overlay can cover the image center;
      // the separate blocking-prompt check handles modal screens.
      if (getComputedStyle(img).visibility !== 'visible' || Number(getComputedStyle(img).opacity) < 0.1) return [];
      return [{ img, area: width*height, source: img.currentSrc, width: r.width, height: r.height }];
    }).sort((a,b) => b.area-a.area);
    const best = candidates[0];
    if (!best) return null;
    document.querySelectorAll('[data-comi-active-image]').forEach(i => i.removeAttribute('data-comi-active-image'));
    best.img.setAttribute('data-comi-active-image', 'true');
    if (!document.querySelector('[data-comi-carousel]')) {
      for (let p = best.img.parentElement; p && p !== document.body; p = p.parentElement) {
        if (p.querySelector('button[aria-label="Next"], [role="button"][aria-label="Next"], button[aria-label="Previous"], [role="button"][aria-label="Previous"]')) {
          p.setAttribute('data-comi-carousel', 'true'); break;
        }
      }
      if (!document.querySelector('[data-comi-carousel]')) (best.img.closest('article') || best.img.parentElement).setAttribute('data-comi-carousel','true');
    }
    return { source: best.source, width: best.width, height: best.height };
  }, scoped);
  return found && { ...found, locator: page.locator('[data-comi-active-image]').first() };
}

async function waitForStableImage(page, { scoped = true, previous = null, timeout = IMAGE_WAIT_TIMEOUT_MS } = {}) {
  const deadline = Date.now() + timeout;
  let candidate = null, stableSince = 0;
  while (Date.now() < deadline) {
    const image = await visibleImage(page, scoped);
    if (image && (!previous || imageIdentity(image.source) !== imageIdentity(previous.source))) {
      // Decode before accepting an image; require the same source and dimensions
      // to persist briefly so lazy loading and layout shifts have settled.
      const decoded = await image.locator.evaluate(img => img.decode().then(() => true, () => false)).catch(() => false);
      const signature = `${imageIdentity(image.source)}:${image.width}x${image.height}`;
      if (decoded && candidate === signature) {
        if (Date.now() - stableSince >= 700) return image;
      } else {
        candidate = decoded ? signature : null;
        stableSince = Date.now();
      }
    } else {
      candidate = null;
    }
    await page.waitForTimeout(250);
  }
  throw new Error(`Timed out waiting ${timeout / 1000}s for a decoded, stable post image`);
}

function imageIdentity(source) {
  try { const url = new URL(source); return url.origin + url.pathname; } catch { return source; }
}

async function navigate(page, direction, previous) {
  const button = page.locator('[data-comi-carousel]').getByRole('button', { name: direction, exact: true }).first();
  if (!(await button.isVisible().catch(() => false)) || await button.isDisabled()) return null;
  await button.click({ timeout: 8000 });
  return waitForStableImage(page, { previous, timeout: IMAGE_WAIT_TIMEOUT_MS });
}

export async function captureCarousel(page, { output, sourceUrl, maxSlides = 20 }) {
  await mkdir(output, { recursive: true });
  const manifest = { source_url: sourceUrl, captured_at: new Date().toISOString(), status: 'blocked', reason: null, popup_dismissals: 0, slides: [] };
  try {
    await page.screenshot({ path: resolve(output, 'before-dismiss.png') });
    manifest.popup_dismissals += await dismissOverlays(page);
    const remainingPrompt = page.getByText(/See this post in the app|Never miss a post|Log in to continue/i).first();
    if (await remainingPrompt.isVisible().catch(() => false)) throw new Error('A blocking Instagram prompt could not be dismissed');
    await page.screenshot({ path: resolve(output, 'after-dismiss.png') });
    if (/\/accounts\/(login|signup)|\/challenge\//.test(new URL(page.url()).pathname)) throw new Error('Instagram requires login or a challenge');
    let current = await waitForStableImage(page, { scoped: false, timeout: IMAGE_WAIT_TIMEOUT_MS });
    // A post URL can open on a later slide. Rewind using the public UI first.
    for (let count = 0; count < maxSlides; count++) {
      const previous = await navigate(page, 'Previous', current);
      if (!previous) break;
      current = previous;
      if (count === maxSlides-1) throw new Error('Could not establish the first carousel slide');
    }
    const seen = new Set();
    for (let index = 1; index <= maxSlides; index++) {
      manifest.popup_dismissals += await dismissOverlays(page);
      current = await waitForStableImage(page, { timeout: IMAGE_WAIT_TIMEOUT_MS });
      const identity = imageIdentity(current.source);
      if (seen.has(identity)) throw new Error('Carousel repeated an image before reaching its end');
      seen.add(identity);
      await page.mouse.move(20, 20);
      const file = resolve(output, `slide-${String(index).padStart(3,'0')}.png`);
      const bytes = await current.locator.screenshot({ path: file, animations: 'disabled', timeout: 10000 });
      manifest.slides.push({ index, file, image_url: current.source, sha256: createHash('sha256').update(bytes).digest('hex'), width: current.width, height: current.height });
      // A missing Next control means a normal end; a broken navigation is partial.
      const nextButton = page.locator('[data-comi-carousel]').getByRole('button', { name: 'Next', exact: true }).first();
      if (!(await nextButton.isVisible().catch(() => false)) || await nextButton.isDisabled()) {
        manifest.status = 'complete'; break;
      }
      if (index === maxSlides) { manifest.status = 'partial'; manifest.reason = 'Configured slide limit reached'; break; }
      current = await navigate(page, 'Next', current);
    }
  } catch (error) {
    manifest.status = manifest.slides.length ? 'partial' : 'blocked';
    manifest.reason = error.message;
    await page.screenshot({ path: resolve(output,'failure.png') }).catch(() => {});
  }
  await writeFile(resolve(output,'manifest.json'), JSON.stringify(manifest,null,2)+'\n');
  return manifest;
}

async function main() {
  const args = process.argv.slice(2);
  const source = args.shift();
  const sourceUrl = normalizePostUrl(source);
  let output = 'data/instagram-captures/' + new URL(sourceUrl).pathname.split('/')[2];
  let maxSlides = 20, headed = false;
  while (args.length) {
    const flag = args.shift();
    if (flag === '--output') output = args.shift();
    else if (flag === '--max-slides') maxSlides = Number(args.shift());
    else if (flag === '--headed') headed = true;
    else throw new Error(`Unknown option: ${flag}`);
  }
  if (!Number.isInteger(maxSlides) || maxSlides < 1 || maxSlides > 50) throw new Error('max-slides must be between 1 and 50');
  output = resolve(output);
  await mkdir(output, { recursive: true });
  const browser = await chromium.launch({ headless: !headed });
  const deadline = setTimeout(() => browser.close().catch(() => {}), 180000);
  try {
    const context = await browser.newContext({ viewport: { width: 1600, height: 1200 }, deviceScaleFactor: 2, locale: 'en-US' });
    const page = await context.newPage();
    let manifest;
    try {
      await page.goto(sourceUrl, { waitUntil: 'domcontentloaded', timeout: 60000 });
      manifest = await captureCarousel(page, { output, sourceUrl, maxSlides });
    } catch (error) {
      manifest = {source_url:sourceUrl,captured_at:new Date().toISOString(),status:'blocked',reason:error.message,popup_dismissals:0,slides:[]};
      await writeFile(resolve(output,'manifest.json'),JSON.stringify(manifest,null,2)+'\n');
    }
    console.log(JSON.stringify(manifest));
    if (manifest.status !== 'complete') process.exitCode = 2;
  } finally { clearTimeout(deadline); await browser.close(); }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
