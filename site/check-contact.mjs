import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const base = process.env.SITE_URL || 'http://127.0.0.1:8797';
const remote = false;
assert(['127.0.0.1', 'localhost'].includes(new URL(base).hostname), 'Contact checks use the local email simulator only');
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage();
  await page.goto(`${base}/contact/`);
  await page.getByLabel('Your name').fill('RetroFeel deployment check');
  // Local delivery is simulated; remote test is sent only to the configured owner.
  await page.getByLabel('Your email').fill('test@example.com');
  await page.getByLabel('Your message').fill('Website delivery check: RetroFeel contact form, updated app fonts, and Variant Hunter branding. This is a one-time setup test; no reply needed.');
  await page.getByRole('checkbox').check();
  const [response] = await Promise.all([
    page.waitForResponse(r => r.request().method() === 'POST'),
    page.getByRole('button', { name: 'Send message' }).click(),
  ]);
  assert.equal(response.status(), 303, `Delivery not accepted: HTTP ${response.status()}`);
  await page.waitForURL('**/contact/?sent=1');
  assert((await page.locator('#contact-status').innerText()).includes('accepted for email delivery'));
  assert(!/chris@|mailto:/i.test(await page.content()));
  console.log(`PASS: native form POST and success redirect (${remote ? 'real email accepted; confirm receipt separately' : 'local email simulator'}).`);

  if (!remote) {
    const invalid = await page.request.post(`${base}/contact/`, {
      headers: { origin: base }, form: { email: 'bad', name: '"><script>bad</script>', topic: 'privacy', message: '<script>preserve safely</script>', consent: 'yes' },
    });
    assert.equal(invalid.status(), 422);
    const html = await invalid.text();
    assert(html.includes('&lt;script&gt;preserve safely&lt;/script&gt;'));
    await page.setContent(html);
    assert.equal(await page.locator('script').count(), 0);
    assert.equal(await page.locator('#name').inputValue(), '"><script>bad</script>');
    assert.equal(await page.locator('#message').inputValue(), '<script>preserve safely</script>');
    assert.equal(invalid.headers()['cache-control'], 'no-store, no-transform');
    const foreign = await page.request.post(`${base}/contact/`, { headers: { origin: 'https://foreign.example' }, form: { message: 'blocked' } });
    assert.equal(foreign.status(), 403);
    for (let index = 0; index < 6; index++) {
      const response = await page.request.post(`${base}/contact/`, { headers: { origin: base }, form: { message: 'invalid' } });
      if (index === 5) assert.equal(response.status(), 429);
    }
    console.log('PASS: workerd HTML escaping, validation failure, origin rejection and rate-limit enforcement.');
  }
} finally { await browser.close(); }
