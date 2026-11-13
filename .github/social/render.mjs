// Draws card.html at 1200 x 630 and saves it as docs/social-card.png.
//
//   node .github/social/render.mjs
//
// Uses Google Chrome (set CHROME to its path if it is not in the usual place),
// headless, with a new empty profile that is deleted afterwards.
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const card = pathToFileURL(join(here, 'card.html')).href;
const out = resolve(here, '../../docs/social-card.png');
const chromePath = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const port = 9341;
const profile = mkdtempSync(join(tmpdir(), 'social-card-'));
const chrome = spawn(chromePath, [
  '--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`,
  '--no-first-run', '--no-default-browser-check', '--disable-extensions', '--allow-file-access-from-files',
  '--hide-scrollbars', 'about:blank',
], { stdio: 'ignore' });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let ws;
try {
  let target;
  for (let i = 0; i < 50 && !target; i++) {
    await sleep(200);
    try {
      target = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find((t) => t.type === 'page');
    } catch {}
  }
  if (!target) throw new Error('Chrome did not start');
  ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener('open', r, { once: true }));
  let id = 0;
  const waiting = new Map();
  ws.addEventListener('message', (e) => {
    const m = JSON.parse(e.data);
    if (m.id && waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); }
  });
  const send = (method, params = {}) => new Promise((r) => { const n = ++id; waiting.set(n, r); ws.send(JSON.stringify({ id: n, method, params })); });

  await send('Emulation.setDeviceMetricsOverride', { width: 1200, height: 630, deviceScaleFactor: 1, mobile: false });
  await send('Page.navigate', { url: card });
  await sleep(1500);
  const ready = await send('Runtime.evaluate', {
    expression: `Promise.all([...document.images].map(i => i.decode())).then(() => document.fonts.ready).then(() => [...document.images].every(i => i.naturalWidth > 0))`,
    awaitPromise: true, returnByValue: true,
  });
  if (!ready.result?.result?.value) throw new Error('the screenshot in the card did not load');
  const shot = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1200, height: 630, scale: 1 } });
  writeFileSync(out, Buffer.from(shot.result.data, 'base64'));
  console.log(`saved ${out}`);
} finally {
  try { ws && ws.close(); } catch {}
  chrome.kill();
  await sleep(1500);
  rmSync(profile, { recursive: true, force: true });
}
