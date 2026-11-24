// Playwright sidecar: drives one engine (chromium | firefox | webkit).
// Protocol: newline-delimited JSON. Requests on stdin: {"id": n, "cmd": "...", ...}.
// Responses on stdout: {"id": n, "ok": true, "value": ...} or {"id": n, "ok": false, "error": "..."}.
'use strict';

const path = require('path');
const readline = require('readline');

const [, , playwrightDir, engine] = process.argv;
const pw = require(path.resolve(playwrightDir));

let browser, context, page;
let viewport = { width: 1024, height: 768 };

function adopt(p) {
  page = p;
  p.on('close', () => {
    if (page === p) {
      const rest = context.pages();
      page = rest[rest.length - 1];
    }
  });
}

const handlers = {
  async launch() {
    browser = await pw[engine].launch({ headless: true });
    context = await browser.newContext({ viewport, ignoreHTTPSErrors: true });
    // Links that open new tabs replace the visible page.
    context.on('page', adopt);
    adopt(await context.newPage());
    return browser.version();
  },
  async navigate({ url }) {
    await page.goto(url, { waitUntil: 'commit', timeout: 60000 });
  },
  async back() {
    await page.goBack({ waitUntil: 'commit' });
  },
  async forward() {
    await page.goForward({ waitUntil: 'commit' });
  },
  async reload() {
    await page.reload({ waitUntil: 'commit' });
  },
  async resize({ width, height }) {
    viewport = { width, height };
    await Promise.all(context.pages().map((p) => p.setViewportSize(viewport)));
  },
  async click({ x, y }) {
    await page.mouse.click(x, y);
  },
  async hover({ x, y }) {
    await page.mouse.move(x, y);
  },
  async scroll({ x, y, dx, dy }) {
    await page.mouse.move(x, y);
    await page.mouse.wheel(dx, dy);
  },
  async keys({ keys }) {
    for (const k of keys) {
      if (k.text !== undefined) await page.keyboard.type(k.text);
      else await page.keyboard.press(k.key);
    }
  },
  async evaluate({ expr }) {
    return await page.evaluate(expr);
  },
  async screenshot() {
    const buf = await page.screenshot({ type: 'png', timeout: 10000, animations: 'allow', caret: 'initial' });
    return buf.toString('base64');
  },
  async meta() {
    const title = await page.title().catch(() => '');
    return { url: page.url(), title, width: viewport.width, height: viewport.height };
  },
  async quit() {
    await browser?.close().catch(() => {});
    setImmediate(() => process.exit(0));
  },
};

function reply(msg) {
  process.stdout.write(JSON.stringify(msg) + '\n');
}

// Requests run strictly in order; the Rust side waits for each response.
let queue = Promise.resolve();
readline.createInterface({ input: process.stdin }).on('line', (line) => {
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    return;
  }
  queue = queue.then(async () => {
    try {
      const h = handlers[req.cmd];
      if (!h) throw new Error(`unknown command ${req.cmd}`);
      reply({ id: req.id, ok: true, value: (await h(req)) ?? null });
    } catch (e) {
      reply({ id: req.id, ok: false, error: String((e && e.message) || e).split('\n')[0] });
    }
  });
});

process.stdin.on('end', async () => {
  await browser?.close().catch(() => {});
  process.exit(0);
});
