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
  if (url.pathname === '/fai/rpc') {
    let body = '';
    req.setEncoding('utf8');
    req.on('data', (chunk) => {
      body += chunk;
    });
    req.on('end', () => {
      let fn = '';
      try {
        fn = JSON.parse(body).fn ?? '';
      } catch {
        res.writeHead(400, { 'content-type': 'application/json; charset=utf-8' });
        res.end('{"ok":false,"error":"invalid fixture RPC"}');
        return;
      }
      res.writeHead(200, { 'content-type': 'application/json; charset=utf-8' });
      if (fn === 'fixture.fail') {
        res.end('{"ok":false,"error":"fixture failed"}');
      } else if (fn === 'fixture.records') {
        // Return an array of records for exercising RPC array-of-records decode.
        // args = [numRecords, numFields]; each record has fields f0..f{n-1}
        // (even = string, odd = int) so the client can read any field.
        let args = [];
        try {
          args = JSON.parse(body).args ?? [];
        } catch {
          args = [];
        }
        const numRecords = Number(args[0] ?? 1);
        const numFields = Number(args[1] ?? 11);
        const recs = [];
        for (let r = 0; r < numRecords; r++) {
          const o = {};
          for (let f = 0; f < numFields; f++) {
            o[`f${f}`] = f % 2 === 0 ? `r${r}f${f}` : 1000 + f;
          }
          recs.push(o);
        }
        res.end(JSON.stringify({ ok: true, value: recs }));
      } else {
        res.end('{"ok":true,"value":null}');
      }
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

  const search = assertion.ownership === 'balanced' ? '?fai_ownership_check=1' : '';
  await page.goto(`http://127.0.0.1:${port}/index.html${search}`, { waitUntil: 'networkidle' });

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

  if (assertion.rootResult !== undefined) {
    await page.waitForFunction(() => window.__FAI_ROOT_DONE === true, null, {
      timeout: assertion.timeoutMs ?? 5000,
    });
    const actual = await page.evaluate(() => window.__FAI_ROOT_RESULT_TEXT ?? '');
    if (String(actual).trim() !== assertion.rootResult.trim()) {
      throw new Error(`root result mismatch\nexpected:\n${assertion.rootResult}\nactual:\n${actual}`);
    }
  }

  if (assertion.durationLessThanMs !== undefined || assertion.durationAtLeastMs !== undefined) {
    await page.waitForFunction(() => window.__FAI_ROOT_DONE === true, null, {
      timeout: assertion.timeoutMs ?? 5000,
    });
    const duration = await page.evaluate(() => {
      if (typeof window.__FAI_ROOT_STARTED_AT !== 'number' || typeof window.__FAI_ROOT_FINISHED_AT !== 'number') {
        return null;
      }
      return window.__FAI_ROOT_FINISHED_AT - window.__FAI_ROOT_STARTED_AT;
    });
    if (duration === null) {
      throw new Error('root duration unavailable');
    }
    if (assertion.durationAtLeastMs !== undefined && duration < assertion.durationAtLeastMs) {
      throw new Error(`root duration too short\nminimum: ${assertion.durationAtLeastMs}ms\nactual: ${duration}ms`);
    }
    if (assertion.durationLessThanMs !== undefined && duration >= assertion.durationLessThanMs) {
      throw new Error(`root duration too long\nmaximum: < ${assertion.durationLessThanMs}ms\nactual: ${duration}ms`);
    }
  }

  if (assertion.clickSelector !== undefined) {
    const count = assertion.clickCount ?? 1;
    await page.waitForSelector(assertion.clickSelector, { timeout: assertion.timeoutMs ?? 5000 });
    for (let i = 0; i < count; i++) {
      await page.locator(assertion.clickSelector).click();
    }
    await page.waitForTimeout(0);
  }

  if (assertion.leak !== undefined) {
    // Browser leak gate (plan 118 U4): after the root completes, read
    // the generated runtime leak dump. This must be a real guest-instrumented
    // run; the dump function exists in all browser runtimes, so reject the
    // explicit "no guest events" hint instead of silently trusting the scalar.
    await page.waitForFunction(() => window.__FAI_ROOT_DONE === true, null, {
      timeout: assertion.timeoutMs ?? 5000,
    });
    const dump = await page.evaluate(() =>
      typeof window.__fai_dump_leaks === 'function' ? window.__fai_dump_leaks() : null
    );
    if (dump === null) {
      throw new Error('leak gate: window.__fai_dump_leaks unavailable — runtime JS predates browser leak diagnostics');
    }
    // Whether the build is leak-instrumented is a BUILD fact — does the
    // wasm import `__fai_alloc_event` — not a runtime event count. An
    // instrumented program that happens to allocate nothing (e.g. a
    // host call taking only data-section string literals) legitimately
    // produces zero guest events and is trivially leak-flat; rejecting
    // on "no guest events" alone false-fails it. Only reject when the
    // import is genuinely absent (someone forgot --check-leaks).
    const wasmBytes = await readFile(join(root, 'main.wasm'));
    const instrumented = WebAssembly.Module.imports(new WebAssembly.Module(wasmBytes)).some(
      (imp) => imp.name === '__fai_alloc_event'
    );
    if (!instrumented) {
      throw new Error(`leak gate: module not built with --check-leaks (no __fai_alloc_event import)\n${dump}`);
    }
    const live = await page.evaluate(() =>
      typeof window.__fai_live_objects === 'function' ? window.__fai_live_objects() : null
    );
    if (live === null) {
      throw new Error('leak gate: window.__fai_live_objects unavailable — runtime JS predates the accessor');
    }
    if (live < 0) {
      throw new Error(`leak gate: __live_objects is NEGATIVE (${live}) — host/guest free imbalance, investigate`);
    }
    if (assertion.leak === 'flat' && live > 0) {
      throw new Error(`marked leak: flat but ${live} object(s) live after root — an unexpected leak (regression)\n${dump}`);
    }
    if (assertion.leak === 'expected' && live === 0) {
      throw new Error(`marked leak: expected ${assertion.leakTag ?? ''} but ran FLAT — the leak is fixed; flip the marker to leak: flat`);
    }
  }

  if (assertion.ownership === 'balanced') {
    await page.waitForFunction(() => window.__FAI_ROOT_DONE === true, null, {
      timeout: assertion.timeoutMs ?? 5000,
    });
    const ownership = await page.evaluate(() =>
      typeof window.__fai_assert_ownership === 'function' ? window.__fai_assert_ownership() : null
    );
    if (ownership === null) {
      throw new Error('ownership gate: window.__fai_assert_ownership unavailable — rebuild with FAI_OWNERSHIP_CHECK=1');
    }
    if (!ownership.ok) {
      const dump = await page.evaluate(() =>
        typeof window.__fai_dump_ownership === 'function' ? window.__fai_dump_ownership() : ''
      );
      throw new Error(`ownership gate: helper imbalance\n${dump}`);
    }
  }

  if (consoleErrors.length > 0) {
    throw new Error(`browser console errors:\n${consoleErrors.join('\n')}`);
  }
} finally {
  await browser.close();
  await new Promise((resolveClose) => server.close(resolveClose));
}
