const TOPICS = { general: 'General question', feedback: 'Playtesting and feedback', privacy: 'Privacy or data request' };
const MAX_BODY_BYTES = 65_536;
function configured(env) {
  const address = value => typeof value === 'string' && value.length <= 254 &&
    /^[a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-zA-Z0-9](?:[a-zA-Z0-9.-]*[a-zA-Z0-9])?\.[a-zA-Z]{2,}$/.test(value);
  return address(env.CONTACT_RECIPIENT) && address(env.CONTACT_SENDER) &&
    typeof env.CONTACT_EMAIL?.send === 'function' &&
    typeof env.CONTACT_LIMIT?.limit === 'function' &&
    typeof env.CONTACT_TOTAL_LIMIT?.limit === 'function';
}

async function readFields(request) {
  if (!request.headers.get('content-type')?.toLowerCase().startsWith('application/x-www-form-urlencoded')) {
    throw new Error('encoding');
  }
  if (Number(request.headers.get('content-length')) > MAX_BODY_BYTES) throw new Error('size');
  const reader = request.body?.getReader();
  if (!reader) throw new Error('empty');
  let size = 0;
  const chunks = [];
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > MAX_BODY_BYTES) { await reader.cancel(); throw new Error('size'); }
      chunks.push(value);
    }
  } finally { reader.releaseLock(); }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
  const fields = new URLSearchParams(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
  const allowed = ['name', 'email', 'topic', 'message', 'website', 'consent'];
  for (const key of fields.keys()) {
    if (!allowed.includes(key) || fields.getAll(key).length !== 1) throw new Error('fields');
  }
  return Object.fromEntries(fields);
}

function valid(fields) {
  return typeof fields.email === 'string' && fields.email.length <= 254 &&
    /^[a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-zA-Z0-9](?:[a-zA-Z0-9.-]*[a-zA-Z0-9])?\.[a-zA-Z]{2,}$/.test(fields.email) &&
    !/[\r\n\u0000-\u001f\u007f]/.test(fields.name || '') && (fields.name || '').length <= 100 &&
    Object.hasOwn(TOPICS, fields.topic || '') && fields.consent === 'yes' &&
    typeof fields.message === 'string' && fields.message.trim().length >= 10 && fields.message.length <= 5000 &&
    !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(fields.message);
}

async function formPage(request, env, status = 200, message = '', fields = {}) {
  const assetURL = new URL('/contact/', request.url);
  const asset = await env.ASSETS.fetch(new Request(assetURL));
  if (!asset.ok) return new Response('Contact is temporarily unavailable. Please try again later.', { status: 503, headers: { 'Cache-Control': 'no-store' } });
  const rewrite = new HTMLRewriter().on('#contact-status', { element(el) { el.setInnerContent(message); } });
  // HTMLRewriter escapes text/attribute values; never interpolate submitted HTML.
  for (const key of ['name', 'email']) {
    rewrite.on(`input[name="${key}"]`, { element(el) { el.setAttribute('value', (fields[key] || '').slice(0, 254)); } });
  }
  rewrite.on('textarea', { element(el) { el.setInnerContent((fields.message || '').slice(0, 5000)); } });
  if (Object.hasOwn(TOPICS, fields.topic || '')) {
    rewrite.on(`option[value="${fields.topic}"]`, { element(el) { el.setAttribute('selected', ''); } });
  }
  const transformed = rewrite.transform(asset);
  const headers = new Headers(transformed.headers);
  headers.set('Cache-Control', 'no-store, no-transform');
  // no-referrer turns a native form POST's Origin into null in browsers.
  // same-origin preserves our CSRF check without sending referrers off-site.
  headers.set('Referrer-Policy', 'same-origin');
  headers.delete('etag');
  headers.delete('content-length');
  if (status === 429) headers.set('Retry-After', '60');
  return new Response(request.method === 'HEAD' ? null : transformed.body, { status, headers });
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname !== '/contact/' && url.pathname !== '/contact') return env.ASSETS.fetch(request);
    if (request.method === 'GET' || request.method === 'HEAD') {
      return formPage(request, env, 200, url.searchParams.get('sent') === '1'
        ? 'Thank you. Your message was accepted for email delivery.' : '');
    }
    if (request.method !== 'POST') return new Response('Method not allowed', { status: 405, headers: { Allow: 'GET, HEAD, POST', 'Cache-Control': 'no-store' } });
    // Browsers supply Origin on native form POSTs; no permissive CORS or referer fallback.
    if (request.headers.get('origin') !== url.origin || request.headers.get('sec-fetch-site') === 'cross-site') {
      return formPage(request, env, 403, 'Please send your message from the RetroFeel contact page.');
    }
    let fields = {};
    try {
      if (!configured(env)) return formPage(request, env, 503, 'Contact delivery is not configured. Please try again later.');
      const ip = request.headers.get('CF-Connecting-IP') || 'local-preview';
      if (!(await env.CONTACT_LIMIT.limit({ key: ip })).success ||
          !(await env.CONTACT_TOTAL_LIMIT.limit({ key: 'contact' })).success) {
        return formPage(request, env, 429, 'Too many attempts. Please wait a minute, then try again.');
      }
      try { fields = await readFields(request); }
      catch { return formPage(request, env, 400, 'We could not read that submission. Please use the form below.'); }
      if (fields.website) return formPage(request, env, 400, 'Please leave the extra website field empty.', fields);
      if (!valid(fields)) return formPage(request, env, 422, 'Check your email, enter a message of 10–5,000 characters, and accept the privacy notice.', fields);
      const text = `RetroFeel website contact\nTopic: ${TOPICS[fields.topic]}\nName: ${fields.name || '(not provided)'}\nReply email: ${fields.email}\n\n${fields.message}\n\nSent with consent through retrofeel.org/contact/.`;
      // Await acceptance before showing success; never retry an ambiguous send automatically.
      const result = await env.CONTACT_EMAIL.send({
        from: { email: env.CONTACT_SENDER, name: 'RetroFeel contact' }, to: env.CONTACT_RECIPIENT,
        replyTo: fields.email, subject: `RetroFeel: ${TOPICS[fields.topic]}`, text,
        html: `<pre>${text.replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[char])}</pre>`,
      });
      console.log(JSON.stringify({ event: 'contact.accepted', messageId: result.messageId }));
      return new Response(null, { status: 303, headers: { Location: '/contact/?sent=1', 'Cache-Control': 'no-store' } });
    } catch {
      // Provider errors can contain addresses/content: keep both logs and responses generic.
      console.error(JSON.stringify({ event: 'contact.unavailable' }));
      return formPage(request, env, 503, 'We could not confirm delivery. Your text is kept below. Please try again later; avoid repeated retries in case your first message arrived.', fields);
    }
  },
};
