// Inspector number fields in a real browser: arithmetic (+ - * / and
// parentheses, relative entries such as "*2"), arrow-key steps, the "Keep
// aspect ratio" flag (one cue, several cues, a virtual display), and the
// timeline tabs in stacking order.
//
// Needs Node with Playwright (`npm i -g playwright`, or any project that has
// it) and a Director plus a Producer running on this machine:
//
//   target/debug/lc-director --state-dir /tmp/lcplay-fields &
//   target/debug/lc-producer --browser &
//   node crates/lc-producer/ui-tests/fields.js [screenshot.png]
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
  await p.goto('http://127.0.0.1:7404');
  await p.waitForFunction(() => S.connected && S.doc);
  await p.evaluate(() => {
    edit(() => { S.doc = defaultDoc(); S.sel = null; S.tl = MAIN; });
    S.replaceNext = true; sendDoc();
  });
  await p.waitForTimeout(500);

  // The parser on its own.
  const ev = (t, cur) => p.evaluate(([t, cur]) => evalNumber(t, cur), [t, cur]);
  const cases = [
    ['1920/2', null, 960], ['(1080 - 40) / 2', null, 520], ['3*1.5', null, 4.5], ['1,5*2', null, 3],
    ['-10', 5, -10], ['*2', 5, 10], ['/4', 10, 2.5], ['+10', 5, 15], ['+-3', 5, 2], ['0.1+0.2', null, 0.3],
    ['2*(3+4)', null, 14], ['-(2+3)', null, -5], ['1e3/4', null, 250], ['', 5, null],
  ];
  for (const [t, cur, want] of cases) {
    const got = await ev(t, cur);
    assert(got === want, `"${t}"${cur != null ? ` on ${cur}` : ''} = ${want} (got ${got})`);
  }
  for (const bad of ['abc', '2**3', '(1+2', '1/0', '*2', '3 4', 'alert(1)']) {
    const got = await ev(bad, bad === '*2' ? null : 1);
    assert(Number.isNaN(got), `"${bad}" is refused (got ${got})`);
  }

  // ── One cue: arithmetic in the inspector shows the result, is one step.
  await p.evaluate(() => addCue({ name: 'a', source: { type: 'color', rgba: [1, 0, 0, 1] }, duration_s: 10, x: 0, y: 0, w: 640, h: 360 }, MAIN, 0, 2));
  const row = (label) => p.locator('#inspector .row', { hasText: label });
  const cue = () => p.evaluate(() => selectedCue());
  const x = row('X / Y').locator('input').nth(0), y = row('X / Y').locator('input').nth(1);
  const w = row('W / H').locator('input').nth(0), h = row('W / H').locator('input').nth(1);
  await x.fill('1920/2'); await x.press('Enter');
  assert((await cue()).x === 960 && (await x.inputValue()) === '960', 'X = 1920/2 is 960, and the field shows 960');
  await y.fill('(1080-40)/2'); await y.press('Enter');
  assert((await cue()).y === 520, 'Y = (1080-40)/2 is 520');
  await y.fill('+10'); await y.press('Enter');
  assert((await cue()).y === 530, '"+10" adds to the value');
  await y.fill('+-30'); await y.press('Enter');
  assert((await cue()).y === 500, '"+-30" takes 30 off');
  await y.fill('abc'); await y.press('Enter');
  assert((await cue()).y === 500 && (await y.inputValue()) === '500', 'something unreadable goes back to the value');
  const start = row('Start (s)').locator('input');
  await start.fill('1,5*2'); await start.press('Enter');
  assert((await cue()).start_s === 3, 'a comma is a decimal point');
  await start.click();
  await p.keyboard.press('ArrowUp');
  assert((await cue()).start_s === 3.1 && (await start.inputValue()) === '3.1', 'ArrowUp steps by the field\'s step (0.1)');
  await p.keyboard.press('Shift+ArrowUp');
  assert((await cue()).start_s === 4.1, 'Shift+ArrowUp: ten steps');
  await p.keyboard.press('ArrowDown');
  assert((await cue()).start_s === 4 && (await start.inputValue()) === '4', 'ArrowDown, no float noise');

  // ── Keep aspect ratio.
  await w.fill('1280'); await w.press('Enter');
  assert((await cue()).w === 1280 && (await cue()).h === 360, 'without the flag, W alone changes');
  await row('Keep aspect ratio').locator('input').check();
  assert((await cue()).keep_aspect === true, 'the flag is set on the cue');
  await w.fill('*0.5'); await w.press('Enter');
  let c = await cue();
  assert(c.w === 640 && c.h === 180 && (await h.inputValue()) === '180', `with it, H follows W (${c.w}×${c.h})`);
  await h.fill('360'); await h.press('Enter');
  c = await cue();
  assert(c.w === 1280 && c.h === 360 && (await w.inputValue()) === '1280', `and W follows H (${c.w}×${c.h})`);

  // ── Several cues: "*2" doubles each one's own size; aspect kept per cue.
  await p.evaluate(() => {
    const a = selectedCue();
    addCue({ name: 'b', source: { type: 'color', rgba: [0, 1, 0, 1] }, duration_s: 10, x: 0, y: 0, w: 300, h: 100 }, MAIN, 0, 20);
    const bb = selectedCue();
    edit(() => { bb.keep_aspect = true; });
    select(cueSelection([a.id, bb.id], a.id));
  });
  const mw = row('W / H').locator('input').nth(0);
  assert((await mw.inputValue()) === '' && (await mw.getAttribute('placeholder')) === 'mixed', 'differing widths show as mixed');
  await mw.fill('*2'); await mw.press('Enter');
  const sizes = await p.evaluate(() => selectedCues().map((q) => [q.w, q.h]));
  assert(JSON.stringify(sizes) === '[[2560,720],[600,200]]', `"*2" on mixed values doubles each, keeping each one's aspect (${JSON.stringify(sizes)})`);
  await mw.fill('500'); await mw.press('Enter');
  const sizes2 = await p.evaluate(() => selectedCues().map((q) => [q.w, q.h]));
  assert(JSON.stringify(sizes2) === '[[500,141],[500,167]]', `an absolute W is set on all, each H in its own proportion (${JSON.stringify(sizes2)})`);

  // ── Virtual display: area and resolution keep their aspect.
  await p.evaluate(() => addVirtual());
  await row('Keep aspect ratio').locator('input').check();
  const areaW = p.locator('#inspector .row', { hasText: 'W / H' }).nth(0).locator('input').nth(0);
  const resW = p.locator('#inspector .row', { hasText: 'W / H' }).nth(1).locator('input').nth(0);
  await areaW.fill('960'); await areaW.press('Enter');
  await resW.fill('/2'); await resW.press('Enter');
  const v = await p.evaluate(() => S.doc.stage.virtuals[0]);
  assert(v.canvas_rect.w === 960 && v.canvas_rect.h === 540 && v.width === 960 && v.height === 540,
    `a virtual display's area and resolution keep their aspect (${v.canvas_rect.w}×${v.canvas_rect.h}, ${v.width}×${v.height})`);

  // ── Timeline tabs: lowest on the left, highest on the right.
  await p.click('#btn-task'); await p.click('#btn-task');
  const tabs = await p.$$eval('#tl-tabs .tab', (n) => n.map((x) => x.textContent));
  assert(JSON.stringify(tabs) === '["Main","Task 1","Task 2","+"]', `tabs from the back (Main) to the front-most task (${tabs})`);
  assert(JSON.stringify(await p.evaluate(() => S.doc.tasks.map((t) => t.name))) === '["Task 2","Task 1"]', '(the Tasks window lists the front-most first)');

  await p.waitForTimeout(600);
  assert(!(await p.evaluate(() => S.dirty)) && (await p.evaluate(() => S.server.stage.virtuals[0].keep_aspect)) === true, 'the Director accepted everything, flags included');
  if (process.argv[2]) await p.screenshot({ path: process.argv[2] });
  assert(!errs.length, `no page errors ${errs.join(' | ')}`);
  await b.close();
})();
