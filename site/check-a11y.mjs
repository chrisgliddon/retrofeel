import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import AxeBuilder from '@axe-core/playwright';

const base = process.env.SITE_URL || 'http://127.0.0.1:8797';
const browser = await chromium.launch({ headless: true });
try {
  const context = await browser.newContext();
  const page = await context.newPage();
  for (const width of [320, 390, 768, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    for (const path of ['/', '/privacy/', '/terms/', '/contact/', '/licenses/']) {
      await page.goto(`${base}${path}`, { waitUntil: 'networkidle' });
      await page.evaluate(() => document.fonts.ready);
      const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa', 'wcag22aa']).analyze();
      assert.deepEqual(result.violations.map(v => ({ id: v.id, impact: v.impact,
        nodes: v.nodes.map(n => ({ target: n.target, summary: n.failureSummary })) })), [], `${width}px ${path}`);
      const small = await page.evaluate(() => [...document.querySelectorAll('body *')].flatMap(el => {
        const style = getComputedStyle(el);
        const directText = [...el.childNodes].some(n => n.nodeType === Node.TEXT_NODE && n.textContent.trim());
        if (!directText || !el.getClientRects().length || el.closest('[aria-hidden="true"], .form-trap') || ['STYLE', 'SCRIPT'].includes(el.tagName)) return [];
        return parseFloat(style.fontSize) < 14 ? [{ tag: el.tagName, text: el.textContent.trim().slice(0, 70), size: style.fontSize }] : [];
      }));
      assert.deepEqual(small, [], `${width}px ${path}: text below the 14px project floor`);
      assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${width}px ${path}: page overflow`);
      console.log(`PASS: ${width}px ${path}: axe WCAG A/AA, contrast and 14px text floor`);
    }
  }
  // 200% text resizing must not hide content or force page-level horizontal scrolling.
  await page.setViewportSize({ width: 1280, height: 1000 });
  for (const path of ['/', '/contact/', '/licenses/']) {
    await page.goto(`${base}${path}`, { waitUntil: 'networkidle' });
    await page.evaluate(() => {
      const elements = [...document.querySelectorAll('body *')];
      const sizes = elements.map(el => parseFloat(getComputedStyle(el).fontSize));
      elements.forEach((el, i) => el.style.fontSize = `${sizes[i] * 2}px`);
    });
    assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${path}: 200% text overflow`);
  }
  console.log('PASS: 200% text resizing; browser checks separately cover keyboard, focus and reduced motion.');
} finally { await browser.close(); }
