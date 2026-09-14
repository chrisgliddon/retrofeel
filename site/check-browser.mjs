import assert from 'node:assert/strict';
import { isIP } from 'node:net';
import { readFileSync } from 'node:fs';
import { chromium } from 'playwright';

const base = process.env.SITE_URL || 'http://127.0.0.1:8797';
const resolveIP = process.env.SITE_RESOLVE_IP;
assert(!resolveIP || isIP(resolveIP), 'SITE_RESOLVE_IP must be an IP address');
const browser = await chromium.launch({ headless: true, args: resolveIP ?
  [`--host-resolver-rules=MAP ${new URL(base).hostname} ${resolveIP}`] : [] });
try {
  const page = await browser.newPage();
  const errors = [];
  const requests = new Set();
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => {
    if (message.type() === 'error' && !message.text().includes('404')) errors.push(message.text());
  });
  page.on('request', request => requests.add(new URL(request.url()).origin));
  for (const width of [320, 390, 768, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    for (const path of ['/', '/privacy/', '/terms/', '/contact/', '/licenses/']) {
      const response = await page.goto(`${base}${path}`, { waitUntil: 'networkidle' });
      assert.equal(response.status(), 200, `${width} ${path}: HTTP status`);
      assert.equal(await page.locator('h1').count(), 1);
      assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth),
        `${width} ${path}: horizontal overflow`);
      assert(await page.locator('img').evaluateAll(images => images.every(img => img.complete && img.naturalWidth > 0)),
        `${width} ${path}: broken image`);
      assert.equal(await page.locator('script, iframe').count(), 0);
      assert.equal(await page.locator('form').count(), path === '/contact/' ? 1 : 0);
      await page.evaluate(() => document.fonts.ready);
      assert(await page.evaluate(() => document.fonts.check('16px Barlow') && document.fonts.check('600 32px "Barlow Condensed"')));
      assert.equal(await page.locator('body').evaluate(el => getComputedStyle(el).fontFamily), 'Barlow, system-ui, sans-serif');
      assert.equal(await page.locator('.brand').innerText(), 'RetroFeel.');
      assert(!/mailto:|chris@/i.test(await page.content()));
      assert((await response.allHeaders())['content-security-policy']?.includes("default-src 'none'"));
    }
    console.log(`PASS: ${width}px homepage, privacy, terms, contact and licenses; app fonts, layout, CSP, no mailbox leaks or scripts.`);
  }
  const expectedRows = readFileSync(new URL('../LICENSE.md', import.meta.url), 'utf8').split('\n')
    .filter(line => line.startsWith('| ') && !line.startsWith('| Core |'))
    .map(line => line.slice(2, -2).split(' | '));
  const displayedRows = await page.locator('.license-table tbody tr').evaluateAll(rows =>
    rows.map(row => [...row.cells].map(cell => cell.textContent.trim())));
  assert.deepEqual(displayedRows, expectedRows, 'Every license tracker cell must match the repository');
  console.log(`PASS: all ${expectedRows.length} license tracker rows match source exactly.`);
  await page.goto(base);
  await page.keyboard.press('Tab');
  assert.equal(await page.locator(':focus').innerText(), 'Skip to content');
  await page.keyboard.press('Enter');
  assert.equal(await page.locator(':focus').getAttribute('id'), 'main');
  await page.emulateMedia({ reducedMotion: 'reduce' });
  assert.equal(await page.evaluate(() => getComputedStyle(document.documentElement).scrollBehavior), 'auto');
  const missing = await page.goto(`${base}/not-a-real-page`);
  assert.equal(missing.status(), 404);
  assert((await page.locator('h1').innerText()).length > 0);
  assert.deepEqual(errors, []);
  assert.deepEqual([...requests], [new URL(base).origin], 'Unexpected third-party browser requests');
  if (process.env.SCREENSHOT_DIR) {
    for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(base, { waitUntil: 'networkidle' });
      await page.screenshot({ path: `${process.env.SCREENSHOT_DIR}/retrofeel-${width}.png`, fullPage: true });
    }
  }
  console.log('PASS: keyboard skip link, reduced motion, custom 404, no third-party requests or browser errors.');
} finally {
  await browser.close();
}
