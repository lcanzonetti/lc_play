// Uploads in the Assets window, in a real browser: each file is listed at
// once, greyed out, with a progress bar under it; files go one at a time;
// a stored file becomes a normal library item; ✕ stops an upload (and the
// Asset Manager keeps nothing of it); a refused file stays listed in red
// with its reason, ↻ to send it again and ✕ to remove it.
//
// Needs Node with Playwright, ffmpeg, and an Asset Manager, a Director and a
// Producer running on this machine, the Asset Manager on an EMPTY media
// folder that this script can read:
//
//   LCMS_BIND=127.0.0.1:7403 LCMS_MEDIA_DIR=/tmp/lcplay-up/media target/debug/lc-assets &
//   target/debug/lc-director --state-dir /tmp/lcplay-up/dir &
//   target/debug/lc-producer --browser &
//   node crates/lc-producer/ui-tests/uploads.js /tmp/lcplay-up/media [screenshot.png]
//
// The browser's upload speed is throttled to 1 MB/s so that progress can be
// watched. Exit code 1 on any failure.
const { chromium } = require('playwright');
const { execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const assert = (c, m) => { if (!c) { console.log('FAIL', m); process.exitCode = 1; } else console.log('ok  ', m); };

const media = process.argv[2];
if (!media) { console.log('usage: node uploads.js <Asset Manager media folder> [screenshot.png]'); process.exit(2); }

// Test files: two 3 MB clips (3 s each at 1 MB/s), a still, and a file the
// Asset Manager refuses.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'lcplay-uploads-'));
const clip = (name) => {
  execFileSync('ffmpeg', ['-loglevel', 'error', '-y', '-f', 'lavfi', '-i', 'testsrc2=size=1280x720:rate=30',
    '-t', '4', '-c:v', 'libx264', '-b:v', '6M', path.join(dir, name)]);
  return path.join(dir, name);
};
const a = clip('up-a.mp4'), b = clip('up-b.mp4'), c = clip('up-c.mp4');
// Given as buffers: Playwright cannot mix those with paths in one call.
const txt = { name: 'up-notes.txt', mimeType: 'text/plain', buffer: Buffer.from('not media\n') };
// Refused too, but big enough that the refusal comes while it is being sent.
const big = { name: 'up-big.xyz', mimeType: 'application/octet-stream', buffer: Buffer.alloc(3e6) };
const buf = (file, name) => ({ name: name || path.basename(file), mimeType: 'video/mp4', buffer: fs.readFileSync(file) });

(async () => {
  const br = await chromium.launch();
  const p = await br.newPage({ viewport: { width: 1600, height: 1000 } });
  const errs = [];
  p.on('pageerror', (e) => errs.push(e.message));
  await p.goto('http://127.0.0.1:7404');
  await p.waitForFunction(() => Array.isArray(S.library));
  const cdp = await p.context().newCDPSession(p);
  await cdp.send('Network.enable');
  await cdp.send('Network.emulateNetworkConditions', { offline: false, latency: 0, downloadThroughput: -1, uploadThroughput: 1e6 });

  const rows = () => p.$$eval('#asset-list .asset.upload', (n) => n.map((x) => ({
    name: x.querySelector('.name').textContent,
    state: [...x.classList].find((k) => k.startsWith('up-'))?.slice(3),
    status: x.querySelector('.up-status').textContent.replace(/\u00a0/g, ' '),
    width: parseFloat(x.querySelector('.bar .fill').style.width) || 0,
    opacity: parseFloat(getComputedStyle(x.querySelector('.name').parentElement).opacity),
    retry: !x.querySelector('.btns button:first-child').hidden,
  })));
  const library = () => p.evaluate(() => S.library.map((i) => i.name));

  // ── Three files at once: all listed straight away, greyed, one sending.
  await p.setInputFiles('#file-upload', [a, b, c]);
  let r = await rows();
  assert(r.map((x) => x.name).join() === 'up-a.mp4,up-b.mp4,up-c.mp4', `all three are listed at once (${r.map((x) => x.name)})`);
  assert(r.map((x) => x.state).join() === 'sending,waiting,waiting', `one at a time: the first is sent, the others wait (${r.map((x) => x.state)})`);
  assert(r.every((x) => x.opacity < 1), 'upload rows are greyed out');
  assert(r[1].status.startsWith('waiting to upload'), `a queued file says so (${r[1].status})`);
  const draggable = await p.$eval('#asset-list .asset.upload', (x) => x.getAttribute('draggable'));
  assert(draggable !== 'true', 'an upload row cannot be dragged onto the timeline');

  await p.waitForTimeout(1500);
  r = await rows();
  assert(r[0].width > 10 && r[0].width < 100, `the bar under the first file moves (${r[0].width} %)`);
  assert(/uploading \d+ % · .*MB \/ .*MB/.test(r[0].status), `with a percentage and amounts (${r[0].status})`);
  if (process.argv[3]) await p.screenshot({ path: process.argv[3], clip: await p.locator('#win-assets').boundingBox() });

  // ── ✕ on a waiting file removes it without sending anything.
  await p.click('#asset-list .asset.upload:has-text("up-c.mp4") .btns button.danger');
  r = await rows();
  assert(!r.some((x) => x.name === 'up-c.mp4'), 'a waiting upload is removed by ✕');

  // ── The first one finishes: it becomes a normal item, the next starts.
  await p.waitForFunction(() => S.library.some((i) => i.name === 'up-a.mp4'), null, { timeout: 15000 });
  await p.waitForFunction(() => !S.uploads.some((u) => u.name === 'up-a.mp4'));
  r = await rows();
  assert(r.length === 1 && r[0].name === 'up-b.mp4' && r[0].state === 'sending', `the next file is being sent (${JSON.stringify(r.map((x) => [x.name, x.state]))})`);
  const normal = await p.$eval('#asset-list .asset:not(.upload)', (x) => ({ name: x.querySelector('.name').textContent, drag: x.getAttribute('draggable') }));
  assert(normal.name === 'up-a.mp4' && normal.drag === 'true', 'the stored file is a normal, draggable item');

  // ── ✕ while sending: stopped, removed, nothing kept on the Asset Manager.
  await p.waitForFunction(() => S.uploads[0]?.sent > 5e5);
  await p.click('#asset-list .asset.upload:has-text("up-b.mp4") .btns button.danger');
  assert((await rows()).length === 0, 'a running upload is removed by ✕');
  await p.waitForTimeout(1500);
  await p.evaluate(() => loadLibrary());
  assert(!(await library()).includes('up-b.mp4'), 'the cancelled file is not in the library');
  const left = fs.readdirSync(media);
  assert(!left.some((f) => f.includes('up-b.mp4')), `and nothing of it is left in the media folder (${left})`);

  // ── A refused file: stays, in red, with the reason; ↻ and ✕.
  await p.setInputFiles('#file-upload', [txt]);
  await p.waitForFunction(() => S.uploads[0]?.state === 'failed', null, { timeout: 10000 });
  r = await rows();
  assert(r[0].state === 'failed' && /not a supported media type/.test(r[0].status), `a refused file shows the reason (${r[0].status})`);
  assert(r[0].retry, 'with ↻ to try again');
  await p.click('#asset-list .asset.upload .btns button:first-child');
  await p.waitForFunction(() => S.uploads[0]?.state === 'failed', null, { timeout: 10000 });
  assert(true, '↻ sends it again (refused again)');
  await p.click('#asset-list .asset.upload .btns button.danger');
  assert((await rows()).length === 0, 'and ✕ removes it');
  await p.setInputFiles('#file-upload', [big]);
  await p.waitForFunction(() => S.uploads[0]?.state === 'failed', null, { timeout: 10000 });
  r = await rows();
  assert(/not a supported media type/.test(r[0].status), `a big refused file shows the Asset Manager's reason too (${r[0].status})`);
  await p.click('#asset-list .asset.upload .btns button.danger');

  // ── A failed upload does not hold up the queue.
  await p.setInputFiles('#file-upload', [txt, buf(a, 'up-d.mp4')]);
  await p.waitForFunction(() => S.uploads.length === 1 && S.uploads[0].state === 'failed', null, { timeout: 15000 });
  assert((await library()).includes('up-d.mp4'), 'the file after a refused one is still uploaded');
  await p.click('#asset-list .asset.upload .btns button.danger');

  // ── Sending a name the library already has says it replaces that file.
  await p.setInputFiles('#file-upload', [a]);
  const note = await p.$eval('#asset-list .asset.upload', (x) => x.querySelector('.up-note')?.textContent);
  assert(note === 'replaces the file of the same name', `a file that replaces another says so (${note})`);
  await p.waitForFunction(() => !S.uploads.length, null, { timeout: 15000 });

  // ── Clean up the Asset Manager.
  await p.evaluate(async () => {
    for (const i of S.library.filter((x) => x.name.startsWith('up-'))) await fetch(`${S.assetsUrl}/api/items/${i.id}`, { method: 'DELETE' });
  });
  assert(errs.length === 0, `no page errors (${errs.join('; ')})`);
  await br.close();
  fs.rmSync(dir, { recursive: true, force: true });
})().catch((e) => { console.log('FAIL', e.message); process.exitCode = 1; process.exit(1); });
