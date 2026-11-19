// Virtual displays in the Producer, in a real browser: creating one, its
// inspector, a cue showing it (accepted by the Director), the stage preview
// of what it captures, a loop that must not hang the page, and deleting it
// with the cues that show it.
//
// Needs Node with Playwright (`npm i -g playwright`, or any project that has
// it) and a Director plus a Producer running on this machine:
//
//   target/debug/lc-director --state-dir /tmp/lcplay-vd &
//   target/debug/lc-producer --browser &
//   node crates/lc-producer/ui-tests/virtuals.js [screenshot.png]
//
// It replaces the Director's show with a fresh one. Exit code 1 on any
// failure.
const { chromium } = require('playwright');
const assert = (c, m) => { if (!c) { console.log('FAIL', m); process.exitCode = 1; } else console.log('ok  ', m); };
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage({ viewport: { width: 1600, height: 1000 } });
  const errs = [];
  p.on('pageerror', (e) => errs.push(e.message));
  // Console errors too, except failed requests (the Asset Manager need not
  // be running for this test).
  p.on('console', (m) => { if (m.type() === 'error' && !/Failed to load resource/.test(m.text())) errs.push(m.text()); });
  await p.goto('http://127.0.0.1:7404');
  await p.waitForFunction(() => S.connected && S.doc);
  await p.evaluate(() => {
    edit(() => { S.doc = defaultDoc(); S.sel = null; S.tl = MAIN; });
    S.replaceNext = true; sendDoc();
  });
  await p.waitForTimeout(500);
  const toasts = [];
  await p.exposeFunction('noteToast', (t) => toasts.push(t));
  await p.evaluate(() => { const orig = toast; window.toast = (m, i) => { noteToast(m); orig(m, i); }; });

  // ── Create one from the Show inspector.
  await p.click('#inspector button:has-text("+ Virtual display")');
  const v = await p.evaluate(() => S.doc.stage.virtuals[0]);
  assert(v && v.name === 'Virtual 1' && v.width === 1920 && v.height === 1080, `a virtual display is added (${JSON.stringify(v)})`);
  assert((await p.evaluate(() => S.sel)).type === 'virtual', 'and selected');
  assert((await p.textContent('#inspector h3')) === 'Virtual display', 'its inspector is shown');

  // ── Its inspector: move its area (Enter commits), set the resolution.
  const row = (label, n = 0) => p.locator('#inspector .row', { hasText: label }).nth(n).locator('input');
  await row('X / Y').first().fill('4000');
  await row('X / Y').first().press('Enter');
  const wh = p.locator('#inspector .row', { hasText: 'W / H' });
  await wh.nth(0).locator('input').nth(0).fill('400');
  await wh.nth(0).locator('input').nth(0).press('Enter');
  await wh.nth(0).locator('input').nth(1).fill('200');
  await wh.nth(0).locator('input').nth(1).press('Enter');
  await wh.nth(1).locator('input').nth(0).fill('800');
  await wh.nth(1).locator('input').nth(0).press('Enter');
  await wh.nth(1).locator('input').nth(1).fill('400');
  await wh.nth(1).locator('input').nth(1).press('Enter');
  let vv = await p.evaluate(() => S.doc.stage.virtuals[0]);
  assert(vv.canvas_rect.x === 4000 && vv.canvas_rect.w === 400 && vv.canvas_rect.h === 200 && vv.width === 800 && vv.height === 400,
    `area and resolution are edited (${JSON.stringify(vv)})`);
  await p.click('#inspector button:has-text("Resolution = area")');
  vv = await p.evaluate(() => S.doc.stage.virtuals[0]);
  assert(vv.width === 400 && vv.height === 200, 'Resolution = area');

  // ── Content in its area, and a cue showing it.
  await p.evaluate((id) => {
    addCue({ name: 'red', source: { type: 'color', rgba: [1, 0, 0, 1] }, duration_s: 30, x: 4000, y: 0, w: 200, h: 200 }, MAIN, 0, 0);
    select({ type: 'virtual', id });
  }, vv.id);
  await p.click('#inspector button:has-text("+ Cue showing it")');
  const cue = await p.evaluate(() => selectedCue());
  assert(cue && cue.source.type === 'virtual' && cue.source.virtual === vv.id, 'a cue showing it is added and selected');
  assert(cue.w === 400 && cue.h === 200, `at the virtual display's resolution (${cue.w}×${cue.h})`);
  assert((await p.textContent(`#tracks .cue[data-id="${cue.id}"]`)).trim() === 'Virtual 1', 'the timeline names it after the virtual display');
  assert(await p.$(`#tracks .cue.virtual[data-id="${cue.id}"]`), 'and draws it as a virtual cue');
  assert((await p.textContent('#inspector h3')) === 'Cue', 'the cue inspector…');
  assert(await p.$('#inspector select'), '…lets the source be another virtual display');
  await p.waitForTimeout(600);
  const server = await p.evaluate((id) => ({
    v: S.server.stage.virtuals?.length,
    c: S.server.tracks.flatMap((t) => t.cues).find((c) => c.id === id)?.source,
  }), cue.id);
  assert(server.v === 1 && server.c?.type === 'virtual', `the Director accepted both (${JSON.stringify(server)})`);
  assert(!toasts.some((t) => /refus|invalid|error/i.test(t)), `no refusal (${toasts.join(' | ')})`);

  // ── Stage preview: the picture of the virtual display holds the red cue
  //    on its left half, transparent on the right.
  await p.waitForTimeout(200); // a frame or two
  const px = await p.evaluate((id) => {
    const c = virtualCanvases.get(id), g = c.getContext('2d');
    const at = (x, y) => [...g.getImageData(Math.floor(x * c.width), Math.floor(y * c.height), 1, 1).data];
    return { size: [c.width, c.height], left: at(0.25, 0.5), right: at(0.75, 0.5) };
  }, vv.id);
  assert(px.size[0] === 400 && px.size[1] === 200, `preview picture at its resolution (${px.size})`);
  assert(px.left[0] > 250 && px.left[1] < 5 && px.left[3] === 255, `red where the cue is (${px.left})`);
  assert(px.right[3] === 0, `transparent elsewhere (${px.right})`);

  // ── A loop: a cue showing it inside its own area. The page keeps
  //    drawing, and the loop is simply left out of the picture.
  await p.evaluate((id) => {
    addCue({ name: 'loop', source: { type: 'virtual', virtual: id }, duration_s: 30, x: 4200, y: 0, w: 200, h: 200 }, MAIN, 0, 0);
  }, vv.id);
  await p.waitForTimeout(300);
  const after = await p.evaluate((id) => {
    const c = virtualCanvases.get(id), g = c.getContext('2d');
    return [...g.getImageData(Math.floor(0.75 * c.width), 100, 1, 1).data];
  }, vv.id);
  assert(after[3] === 0, `a virtual display does not show itself (${after})`);

  // ── Delete it: asked first, and its cues go with it.
  await p.evaluate((id) => select({ type: 'virtual', id }), vv.id);
  await p.click('#inspector button:has-text("Delete virtual display")');
  assert(/2 cues show this virtual display/.test(await p.textContent('#modal-text')), 'deleting asks, naming the cues');
  await p.click('#modal-no');
  assert((await p.evaluate(() => S.doc.stage.virtuals.length)) === 1, 'Cancel keeps it');
  await p.click('#inspector button:has-text("Delete virtual display")');
  await p.click('#modal-yes');
  const left = await p.evaluate(() => [S.doc.stage.virtuals.length, S.doc.tracks.flatMap((t) => t.cues).map((c) => c.name)]);
  assert(left[0] === 0 && JSON.stringify(left[1]) === '["red"]', `OK deletes it and its cues (${JSON.stringify(left)})`);
  await p.waitForTimeout(600);
  assert((await p.evaluate(() => S.server.stage.virtuals?.length ?? 0)) === 0, 'the Director has it deleted');

  if (process.argv[2]) await p.screenshot({ path: process.argv[2] });
  assert(!errs.length, `no page errors ${errs.join(' | ')}`);
  await b.close();
})();
