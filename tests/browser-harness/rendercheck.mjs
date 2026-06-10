import { chromium } from 'playwright';
const browser = await chromium.launch();
const page = await browser.newPage();
const errs=[];
page.on('pageerror', e=>errs.push(e.message));
await page.goto('http://localhost:3040/', { waitUntil: 'networkidle' }).catch(()=>{});
await new Promise(r=>setTimeout(r,1500));
const appHtml = await page.evaluate(() => {
  const a = document.getElementById('app');
  return a ? a.innerHTML.slice(0, 300) : '(no #app)';
});
const title = await page.evaluate(()=>document.title);
console.log('TITLE:', title);
console.log('APP INNERHTML (first 300):', appHtml);
console.log('PAGEERRORS:', errs.length);
process.exit(0);
