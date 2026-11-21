// Tasks and the clipboard in a real browser: new tasks on top, reordering
// tasks by dragging them in the Tasks window, Ctrl+C / Ctrl+X / Ctrl+V of
// tasks, and of cues between timelines (into a task, and back).
//
// Needs Node with Playwright (`npm i -g playwright`, or any project that has
// it) and a Director plus a Producer running on this machine:
//
//   target/debug/lc-director --state-dir /tmp/lcplay-tasks &
//   target/debug/lc-producer --browser &
//   node crates/lc-producer/ui-tests/tasks.js [screenshot.png]
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
  const names = () => p.evaluate(() => S.doc.tasks.map((t) => t.name));
  const rows = () => p.$$eval('#tasks .task[data-id] .name', (n) => n.map((x) => x.textContent));
  const key = async (combo) => { await p.locator('body').click({ position: { x: 5, y: 995 } }).catch(() => {}); await p.keyboard.press(combo); };

  // ── New tasks go on top (in front).
  await p.click('#btn-task'); await p.click('#btn-task'); await p.click('#btn-task');
  assert(JSON.stringify(await names()) === '["Task 3","Task 2","Task 1"]', `new tasks are added on top (${await names()})`);
  assert(JSON.stringify(await rows()) === '["Task 3","Task 2","Task 1"]', 'the Tasks window lists them front to back');
  assert((await p.textContent('#tasks .task.main')).includes('always at the back'), 'the main timeline is shown behind them');

  // ── Select by clicking a row; drag a row to reorder.
  await p.click('#tasks .task[data-id] >> text=Task 1');
  let sel = await p.evaluate(() => S.sel);
  assert(sel?.type === 'task' && (await p.evaluate((id) => tlObj(id).name, sel.id)) === 'Task 1', 'clicking a task selects it');
  await p.dragAndDrop('#tasks .task[data-id]:has-text("Task 1")', '#tasks .task[data-id]:has-text("Task 3")', { targetPosition: { x: 20, y: 2 } });
  assert(JSON.stringify(await names()) === '["Task 1","Task 3","Task 2"]', `dragging Task 1 above Task 3 makes it front-most (${await names()})`);
  await p.dragAndDrop('#tasks .task[data-id]:has-text("Task 1")', '#tasks .task[data-id]:has-text("Task 2")', { targetPosition: { x: 20, y: 20 } });
  assert(JSON.stringify(await names()) === '["Task 3","Task 2","Task 1"]', `and below Task 2, to the back (${await names()})`);
  await p.waitForTimeout(500);
  assert(JSON.stringify(await p.evaluate(() => S.server.tasks.map((t) => t.name))) === '["Task 3","Task 2","Task 1"]', 'the Director has the new order');
  await p.click('#btn-undo');
  assert(JSON.stringify(await names()) === '["Task 1","Task 3","Task 2"]', 'a reorder is undone in one step');
  await p.click('#btn-redo');

  // ── Copy and paste a task: a copy above the selected one.
  await p.evaluate(() => {
    const t1 = S.doc.tasks.find((t) => t.name === 'Task 1');
    addCue({ name: 'in-task', source: { type: 'color', rgba: [1, 0, 0, 1] }, duration_s: 4 }, t1.id, 0, 2);
    // A control cue on main that starts Task 1.
    edit(() => S.doc.control_cues.push({ id: 'k-start', name: '', at_s: 1, action: 'play', target: t1.id, to_s: 0 }));
    select({ type: 'task', id: t1.id });
  });
  await key('Control+c');
  await key('Control+v');
  assert(JSON.stringify(await names()) === '["Task 3","Task 2","Task 1 copy","Task 1"]', `Ctrl+C, Ctrl+V pastes a copy above it (${await names()})`);
  const copy = await p.evaluate(() => S.doc.tasks[2]);
  const orig = await p.evaluate(() => S.doc.tasks[3]);
  assert(copy.id !== orig.id && copy.tracks[0].cues[0].name === 'in-task' && copy.tracks[0].cues[0].id !== orig.tracks[0].cues[0].id, 'the copy has its own ids and the cues');
  assert((await p.evaluate(() => S.tl)) === copy.id, 'and is opened');

  // ── Cut and paste is a move: same ids, and the control cue that started
  //    it comes back.
  await p.evaluate((id) => select({ type: 'task', id }), orig.id);
  await key('Control+x');
  assert(JSON.stringify(await names()) === '["Task 3","Task 2","Task 1 copy"]', 'Ctrl+X removes it');
  assert((await p.evaluate(() => S.doc.control_cues.length)) === 0, '…and the control cue that started it');
  await p.evaluate(() => select({ type: 'task', id: S.doc.tasks[0].id }));
  await key('Control+v');
  assert(JSON.stringify(await names()) === '["Task 1","Task 3","Task 2","Task 1 copy"]', `Ctrl+V puts it back above the selected task (${await names()})`);
  assert((await p.evaluate(() => S.doc.tasks[0].id)) === orig.id, 'with its own id');
  assert((await p.evaluate((id) => S.doc.control_cues.some((cc) => cc.target === id), orig.id)), 'and the control cue that starts it is back');
  await p.waitForTimeout(600);
  assert((await p.evaluate(() => S.server.tasks.length)) === 4, 'the Director accepted all of it');

  // ── Cues: copy two from a task, paste them into the main timeline at its
  //    playhead, keeping their spacing; cut one and paste it into a task.
  await p.evaluate((id) => {
    addCue({ name: 'second', source: { type: 'color', rgba: [0, 1, 0, 1] }, duration_s: 2 }, id, 0, 8);
    const cues = tlObj(id).tracks[0].cues;
    S.tl = id;
    select(cueSelection(cues.map((c) => c.id), cues[0].id));
  }, orig.id);
  await key('Control+c');
  await p.evaluate(() => { S.tl = MAIN; seek(10); select(null); });
  await p.waitForTimeout(400);
  await key('Control+v');
  const pasted = await p.evaluate(() => S.doc.tracks[0].cues.map((c) => [c.name, c.start_s]));
  assert(JSON.stringify(pasted) === '[["in-task",10],["second",16]]', `cues paste at the playhead, spacing kept (${JSON.stringify(pasted)})`);
  assert((await p.evaluate(() => selCueIds().length)) === 2, 'and are selected');
  await key('Control+x');
  assert((await p.evaluate(() => S.doc.tracks[0].cues.length)) === 0, 'Ctrl+X cuts them');
  await p.evaluate((id) => { S.tl = id; select(null); }, orig.id);
  await key('Control+v');
  const inTask = await p.evaluate((id) => tlObj(id).tracks.flatMap((t) => t.cues).map((c) => c.name), orig.id);
  assert(inTask.length === 4, `and Ctrl+V pastes them into the open task (${inTask})`);
  await p.waitForTimeout(600);
  assert(!(await p.evaluate(() => S.dirty)), 'everything reached the Director');

  // ── Ctrl+C while typing in a field is the browser's own copy.
  await p.evaluate(() => select(null));
  const nameField = p.locator('#inspector .row', { hasText: 'Name' }).locator('input');
  await nameField.click();
  const before = await p.evaluate(() => JSON.stringify(clipboard));
  await p.keyboard.press('Control+c');
  assert((await p.evaluate(() => JSON.stringify(clipboard))) === before, 'Ctrl+C in a text field leaves the cue clipboard alone');

  // ── A stopped task is off stage; clicking its ruler brings it back,
  //    paused on that frame: its cue is shown (on the stage, and inside a
  //    virtual display capturing it).
  await p.evaluate(() => {
    edit(() => { S.doc = defaultDoc(); S.sel = null; S.tl = MAIN; });
    S.replaceNext = true; sendDoc();
    addTask();
    addCue({ name: 'blue', source: { type: 'color', rgba: [0, 0, 1, 1] }, duration_s: 4, x: 3000, y: 0, w: 100, h: 100 }, S.tl, 0, 2);
    addVirtual();
    const v = S.doc.stage.virtuals[0];
    edit(() => { v.canvas_rect = { x: 3000, y: 0, w: 100, h: 100 }; v.width = 100; v.height = 100; });
    addVirtualCue(v, MAIN, 0, 0);
    S.tl = S.doc.tasks[0].id; select(null);
    seek(0); // the main timeline (left at 10 s above): its cue on air
  });
  await p.waitForTimeout(500);
  const taskId = await p.evaluate(() => S.tl);
  assert(!(await p.evaluate((id) => isActive(id), taskId)), 'a task never played is off stage');
  const ruler = await (await p.$('#ruler')).boundingBox();
  await p.mouse.click(ruler.x + 3 * 40 + 1, ruler.y + 5); // 3 s
  await p.waitForTimeout(400);
  const tr = await p.evaluate((id) => currentTransport(id), taskId);
  assert(tr?.active && !tr.playing, `clicking the ruler brings it on stage, paused (${JSON.stringify(tr && { active: tr.active, playing: tr.playing })})`);
  assert(Math.abs((await p.evaluate((id) => showSeconds(id), taskId)) - 3) < 0.05, 'at the clicked time');
  const vid = await p.evaluate(() => S.doc.stage.virtuals[0].id);
  const blue = await p.evaluate((id) => [...virtualCanvases.get(id).getContext('2d').getImageData(50, 50, 1, 1).data], vid);
  assert(blue[2] > 250 && blue[3] === 255, `the cue under the playhead is shown, also inside the virtual display (${blue})`);
  await p.click('#tl-stop');
  await p.waitForTimeout(400);
  assert(!(await p.evaluate((id) => isActive(id), taskId)), 'Stop takes it off stage again');
  const gone = await p.evaluate((id) => [...virtualCanvases.get(id).getContext('2d').getImageData(50, 50, 1, 1).data], vid);
  assert(gone[3] === 0, 'and the virtual display no longer shows it');

  // ── Pause on a stopped task cues it up: on stage, paused at the start.
  await p.click(`#tasks .task[data-id="${taskId}"] button[title^="Pause"]`);
  await p.waitForTimeout(400);
  const cued = await p.evaluate((id) => currentTransport(id), taskId);
  assert(cued?.active && !cued.playing, `Pause brings a stopped task on stage, paused (${JSON.stringify(cued && { active: cued.active, playing: cued.playing })})`);
  assert(Math.abs(await p.evaluate((id) => showSeconds(id), taskId)) < 0.05, 'at the start');
  const back = await p.evaluate((id) => [...virtualCanvases.get(id).getContext('2d').getImageData(50, 50, 1, 1).data], vid);
  assert(back[3] === 0, `the frame at 0 is shown: its cue (from 2 s) is not on air yet (${back})`);
  await p.click('#tl-stop');
  await p.waitForTimeout(400);

  if (process.argv[2]) await p.screenshot({ path: process.argv[2] });
  assert(!errs.length, `no page errors ${errs.join(' | ')}`);
  await b.close();
})();
