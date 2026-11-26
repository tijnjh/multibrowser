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
let quitting = false;

function adopt(p) {
  page = p;
  p.on('close', () => {
    if (page !== p) return;
    const rest = context.pages();
    page = rest[rest.length - 1];
    // The page closed itself (window.close()) and was the last one: keep a page to show.
    if (!page && !quitting) context.newPage().catch(() => {});
  });
  // A crashed page can't be screenshotted or navigated again. Exit, so the app restarts
  // this engine with a fresh browser and reloads the page.
  p.on('crash', () => {
    process.stderr.write(`${engine}: page crashed\n`);
    process.exit(1);
  });
}

/** One readable error message. Playwright's messages can span many lines: drop the
 *  ASCII box around them and the call log, and boil missing-dependency reports down to
 *  the list of what's missing. */
function errorText(e) {
  const lines = String((e && e.message) || e)
    .split('\n')
    .map((l) => l.replace(/[║╔╗╚╝═]/g, '').trim().replace(/\s*\\$/, ''));
  const callLog = lines.indexOf('Call log:');
  const body = (callLog >= 0 ? lines.slice(0, callLog) : lines).filter(Boolean);
  if (body.some((l) => l.includes('missing dependencies to run browsers'))) {
    const libs = body.indexOf('Missing libraries:');
    if (libs >= 0) return `Missing system libraries: ${body.slice(libs + 1).join(', ')}`;
    const apt = body.findIndex((l) => l.includes('apt-get install'));
    if (apt >= 0) {
      const pkgs = [body[apt].replace(/^.*apt-get install\s*/, ''), ...body.slice(apt + 1)]
        .filter((l) => l && !l.includes('Playwright Team'))
        .join(' ');
      return `Missing system packages. Install them with: sudo apt-get install ${pkgs}`;
    }
  }
  return body.join('\n');
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
    quitting = true;
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
      reply({ id: req.id, ok: false, error: errorText(e) });
    }
  });
});

process.stdin.on('end', async () => {
  quitting = true;
  await browser?.close().catch(() => {});
  process.exit(0);
});
