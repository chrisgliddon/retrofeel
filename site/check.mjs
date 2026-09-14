import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';

const root = resolve(import.meta.dirname, 'public');
function files(dir) {
  return readdirSync(dir).flatMap(name => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? files(path) : [path];
  });
}
const built = files(root);
const allowed = new Set(['index.html', '404.html', 'privacy/index.html',
  'terms/index.html', 'contact/index.html', 'licenses/index.html', 'legal/core-licenses.md',
  'legal/retrofeel-license.txt', 'robots.txt', 'sitemap.xml', '_headers', '_redirects',
  'images/tigerwolf-logo.svg', 'images/retrofeel-library.png', 'images/retrofeel-transcription.png', 'fonts/Barlow-Regular.ttf', 'fonts/Barlow-Medium.ttf',
  'fonts/BarlowCondensed-SemiBold.ttf', 'fonts/OFL.txt']);
let total = 0;
for (const path of built) {
  const relative = path.slice(root.length + 1);
  assert(allowed.has(relative) || /^css\/site\.min\.[a-f0-9]+\.css$/.test(relative),
    `Unexpected public file: ${relative}`);
  total += statSync(path).size;
}
assert(total < 850_000, `Site budget exceeded: ${total} bytes`);
for (const relative of ['index.html', 'privacy/index.html', 'terms/index.html', 'contact/index.html', 'licenses/index.html', '404.html']) {
  const html = readFileSync(join(root, relative), 'utf8');
  assert(!/<(?:script|iframe)\b/i.test(html), `${relative}: unexpected active content`);
  assert(!/mailto:|chris@/i.test(html), `${relative}: exposed mailbox or removed branding`);
  assert(html.includes('/contact/'), `${relative}: missing contact link`);
  assert(html.includes('/licenses/'), `${relative}: missing licenses link`);
  assert(html.includes('>RetroFeel<'), `${relative}: incorrect header branding`);
  assert.equal((html.match(/<form\b/g) || []).length, relative === 'contact/index.html' ? 1 : 0);
  assert(html.includes('lang=en-CA') || html.includes('lang="en-CA"'));
  assert.equal((html.match(/<h1(?:\s|>)/g) || []).length, 1, `${relative}: expected one h1`);
  assert(!/TODO|PLACEHOLDER|\[INSERT/i.test(html), `${relative}: unfinished copy`);
  for (const match of html.matchAll(/(?:src|href)=(?:"([^"]+)"|'([^']+)'|([^\s>]+))/g)) {
    const url = new URL(match[1] || match[2] || match[3],
      `https://retrofeel.org/${relative.replace(/index\.html$/, '')}`);
    if (url.origin !== 'https://retrofeel.org') continue;
    let target = join(root, decodeURIComponent(url.pathname));
    if (url.pathname.endsWith('/')) target = join(target, 'index.html');
    assert(statSync(target).isFile(), `${relative}: broken local link ${url.pathname}`);
    if (url.hash) {
      const content = readFileSync(target, 'utf8');
      const id = decodeURIComponent(url.hash.slice(1));
      assert(content.includes(`id="${id}"`) || content.includes(`id=${id}>`) ||
        content.includes(`id=${id} `), `${relative}: missing anchor ${id}`);
    }
  }
}
const logo = readFileSync(join(import.meta.dirname, 'static/images/tigerwolf-logo.svg'));
assert.equal(createHash('sha256').update(logo).digest('hex'),
  createHash('sha256').update(readFileSync(join(root, 'images/tigerwolf-logo.svg'))).digest('hex'));
assert(readFileSync(join(root, '_headers'), 'utf8').includes("default-src 'none'"));
assert(readFileSync(join(root, '_headers'), 'utf8').includes('no-transform'));
assert(readFileSync(join(root, '_headers'), 'utf8').includes("font-src 'self'"));
assert(readFileSync(join(root, '_headers'), 'utf8').includes("form-action 'self'"));
for (const font of ['Barlow-Regular.ttf', 'Barlow-Medium.ttf', 'BarlowCondensed-SemiBold.ttf', 'OFL.txt']) {
  assert.deepEqual(readFileSync(join(root, 'fonts', font)),
    readFileSync(join(import.meta.dirname, '../apps/retrofeel/assets/fonts', font)), `Font drift: ${font}`);
}
assert(readFileSync(join(root, 'index.html'), 'utf8').includes('Better context for agentic coding'));
for (const [source, published] of [['LICENSE.md', 'core-licenses.md'], ['LICENSE', 'retrofeel-license.txt']]) {
  assert.deepEqual(readFileSync(join(root, 'legal', published)),
    readFileSync(join(import.meta.dirname, '..', source)), `License source drift: ${source}`);
}
const trackerRows = readFileSync(join(import.meta.dirname, '../LICENSE.md'), 'utf8')
  .split('\n').filter(line => line.startsWith('| ') && !line.startsWith('| Core |'));
const licensesHTML = readFileSync(join(root, 'licenses/index.html'), 'utf8');
assert.equal((licensesHTML.match(/<tr>/g) || []).length - 1, trackerRows.length, 'Missing core license rows');
assert(licensesHTML.includes('BSD|LGPL'), 'Pipe-delimited license declarations must not become table separators');
assert(readFileSync(join(root, '_redirects'), 'utf8').includes(
  '/privacy.html /privacy/ 301'));
console.log(`PASS: ${built.length} public files, ${total} bytes; links, fonts, branding, private contact and asset contract.`);

assert(!readFileSync(join(root, 'index.html'), 'utf8').includes('github.com/'), 'Repository link must be disabled by default');
