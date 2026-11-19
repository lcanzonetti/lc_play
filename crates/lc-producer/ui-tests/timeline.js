// Timeline editing in a real browser: new tracks on top, box selection and
// Shift/Ctrl-click of several cues, editing several cues at once in the
// inspector, dragging a group, and committing typed values with Enter.
//
// Needs Node with Playwright (`npm i -g playwright`, or any project that has
// it) and a Director plus a Producer running on this machine:
//
//   target/debug/lc-director --state-dir /tmp/lcplay-tl &
//   target/debug/lc-producer --browser &
//   node crates/lc-producer/ui-tests/timeline.js [screenshot.png]
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
  // A fresh show, with the Timeline and Inspector windows where the test can
  // reach them.
  await p.evaluate(() => {
    edit(() => { S.doc = defaultDoc(); S.sel = null; S.tl = MAIN; });
    S.replaceNext = true; sendDoc();
    S.pxPerSec = 40;
  });
  await p.waitForTimeout(500);

  // ── 1. A new track goes on top (index 0: the front-most layer).
  const first = await p.evaluate(() => S.doc.tracks[0].id);
  await p.click('#btn-track');
  const tracks = await p.evaluate(() => S.doc.tracks.map((t) => t.id));
  assert(tracks.length === 2 && tracks[1] === first && tracks[0] !== first, 'a new track is added above the others');
  const heads = await p.$$eval('#tracks .track-head .name', (n) => n.map((x) => x.textContent));
  assert(heads[0] === 'Track 2' && heads[1] === 'Track 1', `the new track is drawn at the top (${heads})`);

  // Three colour cues: two on the top track, one on the second.
  await p.evaluate(() => {
    const cue = (start) => ({ name: 'c' + start, source: { type: 'color', rgba: [1, 0, 0, 1] }, duration_s: 2 });
    addCue(cue(1), MAIN, 0, 1); addCue(cue(5), MAIN, 0, 5); addCue(cue(9), MAIN, 1, 9);
    select(null);
  });
  const ids = await p.evaluate(() => S.doc.tracks.flatMap((t) => t.cues.map((c) => c.id)));

  // ── 2. Box selection: a drag over empty lane space touching the two cues
  //       of the top track (not the third, which is later).
  const lane0 = await (await p.$$('#tracks .lane'))[0].boundingBox();
  const pps = 40;
  await p.mouse.move(lane0.x + 0.5 * pps, lane0.y + 2);
  await p.mouse.down();
  await p.mouse.move(lane0.x + 4 * pps, lane0.y + lane0.height - 2, { steps: 5 });
  assert(await p.$('#tl-box'), 'the selection box is drawn while dragging');
  await p.mouse.move(lane0.x + 6 * pps, lane0.y + lane0.height - 2, { steps: 5 });
  await p.mouse.up();
  let sel = await p.evaluate(() => selCueIds());
  assert(sel.length === 2 && sel.includes(ids[0]) && sel.includes(ids[1]), `box selection picks the two cues it touches (${sel.length})`);
  assert(!(await p.$('#tl-box')), 'the selection box goes away on release');
  assert((await p.$$('#tracks .cue.sel')).length === 2, 'both cues are drawn selected');
  assert((await p.textContent('#inspector h3')) === '2 cues', 'the inspector edits the 2 cues');

  // ── 3. Shift-click adds the third cue; Ctrl-click takes it out again.
  await p.click(`#tracks .cue[data-id="${ids[2]}"]`, { modifiers: ['Shift'] });
  assert((await p.evaluate(() => selCueIds())).length === 3, 'Shift-click adds a cue to the selection');
  await p.click(`#tracks .cue[data-id="${ids[2]}"]`, { modifiers: ['Control'] });
  assert((await p.evaluate(() => selCueIds())).length === 2, 'Ctrl-click takes it out');
  await p.click(`#tracks .cue[data-id="${ids[2]}"]`, { modifiers: ['Shift'] });

  // ── 4. Mixed values show empty; a value + Enter sets it on all of them,
  //       applies at once, keeps the field focused, and is one undo step.
  const startInput = p.locator('#inspector .row', { hasText: 'Start (s)' }).locator('input');
  assert((await startInput.inputValue()) === '' && (await startInput.getAttribute('placeholder')) === 'mixed', 'differing start times show as "mixed"');
  const durInput = p.locator('#inspector .row', { hasText: 'Duration (s)' }).locator('input');
  assert((await durInput.inputValue()) === '2', 'a shared duration is shown');
  const undoBefore = await p.evaluate(() => S.undo.length);
  await durInput.fill('4.5');
  await durInput.press('Enter');
  let durs = await p.evaluate(() => selectedCues().map((c) => c.duration_s));
  assert(durs.length === 3 && durs.every((d) => d === 4.5), `Enter sets the duration on every selected cue (${durs})`);
  assert(await durInput.evaluate((n) => n === document.activeElement), 'the field keeps the focus after Enter');
  assert((await p.$$eval('#tracks .cue.sel', (n) => n.map((x) => x.style.width))).every((w) => w === `${4.5 * pps}px`), 'the timeline shows the new duration at once');
  await startInput.click();
  assert((await p.evaluate(() => S.undo.length)) === undoBefore + 1, 'Enter, then leaving the field, is one undo step');
  await startInput.fill('3');
  await startInput.press('Enter');
  const starts = await p.evaluate(() => selectedCues().map((c) => c.start_s));
  assert(starts.every((s) => s === 3), `same start time for all (${starts})`);
  await startInput.fill('');
  await startInput.press('Enter');
  assert((await startInput.inputValue()) === '3', 'an emptied field goes back to the shared value');
  // Wait for the Director to have the edit (it echoes it back).
  await p.waitForTimeout(600);
  const server = await p.evaluate((i) => S.server.tracks.flatMap((t) => t.cues).filter((c) => i.includes(c.id)).map((c) => [c.start_s, c.duration_s]), ids);
  assert(server.length === 3 && server.every(([s, d]) => s === 3 && d === 4.5), 'the Director has the change for every cue');
  await p.click('#btn-undo'); await p.click('#btn-undo');
  durs = await p.evaluate((i) => S.doc.tracks.flatMap((t) => t.cues).filter((c) => i.includes(c.id)).map((c) => [c.start_s, c.duration_s]), ids);
  assert(JSON.stringify(durs) === JSON.stringify([[1, 2], [5, 2], [9, 2]]), `two undos restore every cue (${JSON.stringify(durs)})`);

  // ── 5. Dragging one cue of the selection moves them all by the same time,
  //       and the one on the second track stays on its track.
  await p.evaluate((i) => select(cueSelection(i, i[0])), ids);
  const c0 = await (await p.$(`#tracks .cue[data-id="${ids[0]}"]`)).boundingBox();
  await p.mouse.move(c0.x + c0.width / 2, c0.y + c0.height / 2);
  await p.mouse.down();
  await p.mouse.move(c0.x + c0.width / 2 + 2 * pps, c0.y + c0.height / 2, { steps: 6 });
  await p.mouse.up();
  const moved = await p.evaluate((i) => i.map((id) => { const h = findCue(id); return [h.cue.start_s, h.ti]; }), ids);
  assert(JSON.stringify(moved) === JSON.stringify([[3, 0], [7, 0], [11, 1]]), `the group moves together (${JSON.stringify(moved)})`);
  assert((await p.evaluate(() => selCueIds())).length === 3, 'the selection stays after a group drag');
  // A plain click on one of them (no movement) selects it alone.
  await p.click(`#tracks .cue[data-id="${ids[1]}"]`);
  sel = await p.evaluate(() => selCueIds());
  assert(sel.length === 1 && sel[0] === ids[1], 'a click on one cue of the group selects it alone');

  // ── 6. Single-cue fields commit on Enter too (no need to click away).
  const nameInput = p.locator('#inspector .row', { hasText: 'Name' }).locator('input');
  await nameInput.fill('renamed');
  await nameInput.press('Enter');
  assert((await p.evaluate((i) => findCue(i).cue.name, ids[1])) === 'renamed', 'Enter commits a text field');
  assert((await p.textContent(`#tracks .cue[data-id="${ids[1]}"]`)) === 'renamed', 'and the timeline shows it at once');
  const opInput = p.locator('#inspector .row', { hasText: 'Opacity' }).locator('input');
  await opInput.fill('40');
  await opInput.press('Enter');
  assert((await p.evaluate((i) => findCue(i).cue.opacity, ids[1])) === 0.4, 'Enter commits a number field');
  // Show inspector (nothing selected): its fields commit on Enter too.
  await p.evaluate(() => select(null));
  const lenInput = p.locator('#inspector .row', { hasText: 'Length (s)' }).locator('input');
  await lenInput.fill('90');
  await lenInput.press('Enter');
  assert((await p.evaluate(() => S.doc.duration_s)) === 90, 'Enter commits the show length');

  // ── 7. A click on an empty lane clears the selection; Delete removes
  //       every selected cue.
  await p.evaluate((i) => select(cueSelection(i, i[0])), ids);
  await p.mouse.click(lane0.x + 0.25 * pps, lane0.y + lane0.height / 2);
  assert((await p.evaluate(() => S.sel)) === null, 'a click on an empty lane deselects');
  await p.evaluate((i) => select(cueSelection(i, i[0])), ids);
  await p.keyboard.press('Delete');
  assert((await p.evaluate(() => S.doc.tracks.flatMap((t) => t.cues).length)) === 0, 'Delete removes every selected cue');

  if (process.argv[2]) await p.screenshot({ path: process.argv[2] });
  assert(!errs.length, `no page errors ${errs.join(' | ')}`);
  await b.close();
})();
