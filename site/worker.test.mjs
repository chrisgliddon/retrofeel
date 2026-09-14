import assert from 'node:assert/strict';
import { test } from 'node:test';
import worker from './worker.mjs';

// Unit tests isolate dispatch, validation and delivery. Browser checks additionally
// exercise the real HTMLRewriter, asset binding and email simulator in workerd.
globalThis.HTMLRewriter = class {
  on() { return this; }
  transform(response) { return response; }
};
const defaults = { email: 'tester@example.com', topic: 'general', message: 'A useful playtesting question.', consent: 'yes', website: '' };
function setup({ limited = false, totalLimited = false, unavailable = false } = {}) {
  const sent = [];
  const env = {
    CONTACT_RECIPIENT: 'maintainer@example.com',
    CONTACT_SENDER: 'contact@example.com',
    ASSETS: { fetch: async () => new Response('<h1>Contact</h1>', { headers: { 'content-security-policy': "default-src 'none'", etag: 'old' } }) },
    CONTACT_LIMIT: { limit: async () => ({ success: !limited }) },
    CONTACT_TOTAL_LIMIT: { limit: async () => ({ success: !totalLimited }) },
    CONTACT_EMAIL: { send: async message => { if (unavailable) throw new Error('PRIVATE PROVIDER DETAIL'); sent.push(message); return { messageId: 'test-only' }; } },
  };
  return { env, sent };
}
function request(fields = defaults, headers = {}) {
  return new Request('https://retrofeel.org/contact/', { method: 'POST', headers: { origin: 'https://retrofeel.org', ...headers }, body: new URLSearchParams(fields) });
}
test('valid message sends once to fixed owner, with visitor only as reply-to', async () => {
  const { env, sent } = setup();
  const response = await worker.fetch(request({ ...defaults, message: '<img src=x onerror=bad> question' }), env);
  assert.equal(response.status, 303);
  assert.equal(response.headers.get('location'), '/contact/?sent=1');
  assert.equal(sent.length, 1);
  assert.equal(sent[0].to, 'maintainer@example.com');
  assert.equal(sent[0].from.email, 'contact@example.com');
  assert.equal(sent[0].replyTo, defaults.email);
  assert(!sent[0].html.includes('<img'));
});
for (const [name, fields] of Object.entries({
  email: { email: 'invalid' }, injection: { email: 'x@example.com\r\nBcc: x@bad.com' },
  name: { name: 'bad\nname' }, longName: { name: 'x'.repeat(101) }, topic: { topic: '__proto__' },
  message: { message: 'short' }, longMessage: { message: 'a'.repeat(5001) }, consent: { consent: 'no' },
})) {
  test(`reject invalid ${name}`, async () => {
    const { env, sent } = setup();
    assert.equal((await worker.fetch(request({ ...defaults, ...fields }), env)).status, 422);
    assert.equal(sent.length, 0);
  });
}
test('reject honeypot, recipient injection and repeated fields', async () => {
  for (const fields of [{ ...defaults, website: 'spam' }, { ...defaults, to: 'spam@example.com' },
    [...Object.entries(defaults), ['email', 'extra@example.com']]]) {
    const { env, sent } = setup();
    assert.equal((await worker.fetch(request(fields), env)).status, 400);
    assert.equal(sent.length, 0);
  }
});
test('reject cross-origin submissions', async () => {
  for (const headers of [{ origin: 'https://bad.example' }, { origin: 'null' }, { 'sec-fetch-site': 'cross-site' }]) {
    const { env, sent } = setup();
    assert.equal((await worker.fetch(request(defaults, headers), env)).status, 403);
    assert.equal(sent.length, 0);
  }
});
test('reject wrong encoding and oversized chunked body', async () => {
  const { env, sent } = setup();
  assert.equal((await worker.fetch(request(defaults, { 'content-type': 'application/json' }), env)).status, 400);
  const stream = new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array(65_537)); controller.close(); } });
  assert.equal((await worker.fetch(new Request('https://retrofeel.org/contact/', {
    method: 'POST', headers: { origin: 'https://retrofeel.org', 'content-type': 'application/x-www-form-urlencoded' }, body: stream, duplex: 'half',
  }), env)).status, 400);
  assert.equal(sent.length, 0);
});
for (const mode of ['limited', 'totalLimited']) test(`${mode} fails closed`, async () => {
  const { env, sent } = setup({ [mode]: true });
  const response = await worker.fetch(request(), env);
  assert.equal(response.status, 429);
  assert.equal(response.headers.get('retry-after'), '60');
  assert.equal(sent.length, 0);
});
test('send failure is not reported as success or leaked', async () => {
  const { env } = setup({ unavailable: true });
  const response = await worker.fetch(request(), env);
  assert.equal(response.status, 503);
  assert.equal(response.headers.get('cache-control'), 'no-store, no-transform');
  assert.equal(response.headers.get('referrer-policy'), 'same-origin');
  assert(!response.headers.has('etag'));
  assert(!(await response.text()).includes('PRIVATE PROVIDER DETAIL'));
});
test('method rejection and HEAD body', async () => {
  const { env } = setup();
  assert.equal((await worker.fetch(new Request('https://retrofeel.org/contact/', { method: 'DELETE' }), env)).status, 405);
  assert.equal(await (await worker.fetch(new Request('https://retrofeel.org/contact/', { method: 'HEAD' }), env)).text(), '');
});

for (const key of ['CONTACT_RECIPIENT', 'CONTACT_SENDER', 'CONTACT_EMAIL', 'CONTACT_LIMIT', 'CONTACT_TOTAL_LIMIT']) {
  test(`unconfigured ${key} fails closed without delivery`, async () => {
    const { env, sent } = setup();
    delete env[key];
    assert.equal((await worker.fetch(request(), env)).status, 503);
    assert.equal(sent.length, 0);
  });
}
test('invalid configured mailbox fails closed', async () => {
  for (const value of ['', 'bad', 'owner@example.com\r\nBcc: other@example.com']) {
    const { env, sent } = setup();
    env.CONTACT_RECIPIENT = value;
    assert.equal((await worker.fetch(request(), env)).status, 503);
    assert.equal(sent.length, 0);
  }
});
