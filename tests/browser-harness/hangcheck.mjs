import { chromium } from 'playwright';

const url = process.argv[2] || 'http://localhost:3040/';
const browser = await chromium.launch();
const page = await browser.newPage();
const logs = [];
page.on('console', m => logs.push(m.text()));
page.on('pageerror', e => logs.push('PAGEERROR: ' + e.message));

await page.goto(url, { waitUntil: 'commit' }).catch(e => logs.push('goto: ' + e.message));
await new Promise(r => setTimeout(r, 1500));

// Print whatever the page logged before any (possibly blocking) probe.
console.log('--- console (' + logs.length + ') ---');
for (const l of logs.slice(0, 80)) console.log(l);

// Responsiveness probe raced against a node-side timeout.
const hung = await Promise.race([
  page.evaluate(() => 1).then(() => false).catch(() => false),
  new Promise(r => setTimeout(() => r(true), 2500)),
]);
console.log('HUNG=' + hung);

process.exit(0);
