import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { chromium } = require('/tmp/pw/node_modules/playwright/index.js');
import fs from 'fs';

const files = ['Main', 'Privatanalys', 'Kontroll', 'Byggstenar'];
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH });
const page = await browser.newPage({ viewport: { width: 820, height: 900 } });
for (const f of files) {
  const src = fs.readFileSync(`${f}.dc.html`, 'utf8');
  const style = src.match(/<helmet>([\s\S]*?)<\/helmet>/)[1];
  const body = src.match(/<\/helmet>\s*([\s\S]*?)<\/x-dc>/)[1];
  await page.setContent(`<!doctype html><html><head><meta charset="utf-8">${style}</head><body>${body}</body></html>`);
  const h = await page.evaluate(() => document.documentElement.scrollHeight);
  console.log(`${f}: ${h}px`);
}
await browser.close();
