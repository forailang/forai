import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, resolve } from 'node:path';

const [, , rootArg, assertionJson] = process.argv;

if (!rootArg || !assertionJson) {
  console.error('usage: node run-fixture.mjs <build-dir> <assertion-json>');
  process.exit(2);
}

const root = resolve(rootArg);
const assertion = JSON.parse(assertionJson);

const mimeTypes = new Map([
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.css', 'text/css; charset=utf-8'],
  ['.wasm', 'application/wasm'],
]);

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', 'http://127.0.0.1');
  if (url.pathname === '/__fai_fixture/http-get') {
    res.writeHead(200, {
      'content-type': 'text/plain; charset=utf-8',
      'x-fai-fixture': 'browser',
    });
    res.end('browser get ok');
    return;
  }
  if (url.pathname === '/__fai_fixture/http-post') {
    let body = '';
    req.setEncoding('utf8');
    req.on('data', (chunk) => {
      body += chunk;
    });
    req.on('end', () => {
      res.writeHead(201, {
        'content-type': 'text/plain; charset=utf-8',
        'x-fai-fixture': 'browser',
      });
      res.end(`browser post ${body}`);
    });
    return;
  }
  const requested = url.pathname === '/' ? '/index.html' : url.pathname;
  const file = join(root, requested.replace(/^\/+/, ''));
  try {
    const body = await readFile(file);
    res.writeHead(200, {
      'content-type': mimeTypes.get(extname(file)) ?? 'application/octet-stream',
    });
    res.end(body);
  } catch {
    res.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' });
    res.end(`not found: ${requested}`);
  }
});

await new Promise((resolveListen) => server.listen(0, '127.0.0.1', resolveListen));
const { port } = server.address();

const browser = await chromium.launch();
const page = await browser.newPage();

try {
  const consoleErrors = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      consoleErrors.push(msg.text());
    }
  });
  page.on('pageerror', (err) => {
    consoleErrors.push(err.message);
  });

  await page.goto(`http://127.0.0.1:${port}/index.html`, { waitUntil: 'networkidle' });

  if (assertion.selector) {
    await page.waitForSelector(assertion.selector, { timeout: assertion.timeoutMs ?? 5000 });
  }

  if (assertion.text !== undefined) {
    const actual = await page.locator(assertion.selector ?? 'body').innerText();
    if (actual.trim() !== assertion.text.trim()) {
      throw new Error(`text mismatch for ${assertion.selector ?? 'body'}\nexpected:\n${assertion.text}\nactual:\n${actual}`);
    }
  }

  if (assertion.html !== undefined) {
    const actual = await page.locator(assertion.selector ?? 'body').innerHTML();
    if (actual.trim() !== assertion.html.trim()) {
      throw new Error(`html mismatch for ${assertion.selector ?? 'body'}\nexpected:\n${assertion.html}\nactual:\n${actual}`);
    }
  }

  if (consoleErrors.length > 0) {
    throw new Error(`browser console errors:\n${consoleErrors.join('\n')}`);
  }
} finally {
  await browser.close();
  await new Promise((resolveClose) => server.close(resolveClose));
}
