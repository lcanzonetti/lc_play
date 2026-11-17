// Two users editing one show at once, in two real browsers.
//
// Needs Node with Playwright (`npm i -g playwright`, or any project that has
// it) and a Director plus a Producer running on this machine:
//
//   target/debug/lc-director --state-dir /tmp/lcplay-mu &
//   target/debug/lc-producer --browser &
//   node crates/lc-producer/ui-tests/multiuser.js [screenshot.png]
//
// Checks: diff/apply round trip (randomised), simultaneous edits converge
// and both survive, presence and the inspector's note, per-user undo, and a
// user who leaves disappearing. Exit code 1 on any failure.
const { chromium } = require('playwright');
const assert = (c, m) => { if (!c) { console.log('FAIL', m); process.exitCode = 1; } else console.log('ok  ', m); };
(async () => {
  const b = await chromium.launch();
  const errs = [];
  const open = async (name) => {
    const ctx = await b.newContext({ viewport: { width: 1400, height: 900 } });
    await ctx.addInitScript((n) => localStorage.setItem('lcplay.user', JSON.stringify({ name: n, color: n === 'Alice' ? '#ff6b6b' : '#4dabf7' })), name);
    const p = await ctx.newPage(); p.on('pageerror', (e) => errs.push(name + ': ' + e.message));
    await p.goto('http://127.0.0.1:7404'); return p;
  };
  const A = await open('Alice'); await A.waitForTimeout(2500);
  const B = await open('Bob'); await B.waitForTimeout(2500);

  // Property test of diff/apply in the page.
  const prop = await A.evaluate(() => {
    let rnd = 1; const r = (n) => { rnd = (rnd * 1103515245 + 12345) % 2147483648; return rnd % n; };
    const base = () => ({ name: 's', tracks: [0,1,2,3].map((i) => ({ id: 't' + i, name: 'T' + i, cues: [0,1,2].map((j) => ({ id: `c${i}${j}`, x: j, rgba: [1, 0, 0, 1] })) })), stage: { displays: [{ id: 'd1', canvas_rect: { x: 0, y: 0, w: 10, h: 10 } }] } });
    for (let it = 0; it < 500; it++) {
      const a = base(), bb = clone(a);
      for (let m = 0; m < 1 + r(5); m++) {
        const t = bb.tracks;
        switch (r(7)) {
          case 0: t.splice(r(t.length + 1), 0, { id: 'n' + it + m, cues: [] }); break;
          case 1: if (t.length) t.splice(r(t.length), 1); break;
          case 2: if (t.length > 1) { const [e] = t.splice(r(t.length), 1); t.splice(r(t.length + 1), 0, e); } break;
          case 3: if (t.length && t[0].cues.length) t[0].cues[r(t[0].cues.length)].x = r(100); break;
          case 4: bb.name = 'n' + r(9); break;
          case 5: delete bb.stage.displays[0].canvas_rect.h; break;
          case 6: if (t.length) t[r(t.length)].cues.reverse(); break;
        }
      }
      const got = applyOps(clone(a), diffDoc(a, bb));
      if (JSON.stringify(got) !== JSON.stringify(bb)) return `mismatch at ${it}: ${JSON.stringify(diffDoc(a, bb))}`;
    }
    return 'ok';
  });
  assert(prop === 'ok', 'diff then apply reproduces the target (500 random edits): ' + prop);

  // Simultaneous edits from both pages.
  await Promise.all([
    A.evaluate(() => edit(() => { S.doc.stage.displays[0].canvas_rect.x = 123; })),
    B.evaluate(() => edit(() => { tlObj(MAIN).tracks.push({ id: 'bobtrack', name: 'Bob track', cues: [] }); S.doc.name = 'Bob named it'; })),
  ]);
  await A.waitForTimeout(1500);
  // Compared with sorted keys: the order of fields in an object can differ
  // between pages (merging keeps an object's existing order) and means nothing.
  const canon = () => JSON.stringify(S.doc, (k, v) => (v && typeof v === 'object' && !Array.isArray(v) ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => (a < b ? -1 : 1))) : v));
  const docs = await Promise.all([A, B].map((p) => p.evaluate(canon)));
  const da = JSON.parse(docs[0]);
  assert(docs[0] === docs[1], 'both pages converge on the same show');
  assert(da.stage.displays[0].canvas_rect.x === 123 && da.name === 'Bob named it' && da.tracks.some((t) => t.id === 'bobtrack'), "Alice's and Bob's changes are both in it");

  // Presence.
  await A.evaluate(() => select({ type: 'display', id: S.doc.stage.displays[0].id }));
  await B.waitForTimeout(800);
  const seen = await B.evaluate(() => [...S.others.values()].map((o) => [o.name, o.selection?.type]));
  assert(seen.some(([n, t]) => n === 'Alice' && t === 'display'), 'Bob sees Alice and what she selected: ' + JSON.stringify(seen));
  await B.evaluate(() => select({ type: 'display', id: S.doc.stage.displays[0].id }));
  await B.waitForTimeout(300);
  const note = await B.locator('#inspector .together').innerText().catch(() => '');
  assert(note.includes('Alice'), 'the inspector says Alice is on it too: ' + note);

  // Alice undoes: only her change goes.
  await A.evaluate(() => stepHistory(S.undo, S.redo));
  await A.waitForTimeout(1200);
  const after = JSON.parse(await B.evaluate(() => JSON.stringify(S.doc)));
  assert(after.stage.displays[0].canvas_rect.x !== 123 && after.tracks.some((t) => t.id === 'bobtrack'), "Alice's undo reverts her edit and keeps Bob's");
  if (process.argv[2]) await B.screenshot({ path: process.argv[2] });

  // Inspector edits keep working after the show has come back (the fields
  // must write into the live show, not a replaced copy).
  await A.evaluate(() => select({ type: 'display', id: S.doc.stage.displays[0].id }));
  const xy = A.locator('#inspector .row.two').nth(0).locator('input');
  await xy.nth(0).fill('111'); await xy.nth(0).press('Tab'); await A.waitForTimeout(800);
  await xy.nth(1).fill('222'); await xy.nth(1).press('Tab'); await A.waitForTimeout(800);
  const r = await B.evaluate(() => S.doc.stage.displays[0].canvas_rect);
  assert(r.x === 111 && r.y === 222, `two inspector edits in a row both arrive (${r.x}, ${r.y})`);

  // An edit lost with the connection is sent again on reconnect.
  await A.evaluate(() => {
    const ws = S.ws, orig = ws.send.bind(ws);
    ws.send = (d) => { if (d.includes('edit_show')) { ws.send = orig; ws.close(); } else orig(d); };
    edit(() => { S.doc.name = 'survives a drop'; });
    sendDoc();
  });
  await A.waitForTimeout(2500);
  assert(await B.evaluate(() => S.doc.name) === 'survives a drop', 'an edit in flight when the link dropped arrives after reconnecting');

  // A whole new show lost with the connection: sent again, nobody stuck.
  await A.evaluate(() => {
    const ws = S.ws, orig = ws.send.bind(ws);
    ws.send = (d) => { if (d.includes('load_show')) { ws.send = orig; ws.close(); } else orig(d); };
    S.replaceNext = true; edit(() => { S.doc = defaultDoc(); S.doc.name = 'A new show'; }); sendDoc();
  });
  await A.waitForTimeout(2500);
  await B.evaluate(() => edit(() => { S.doc.name = 'B renamed'; }));
  await A.waitForTimeout(1500);
  const names = await Promise.all([A, B].map((p) => p.evaluate(() => [S.doc.name, S.holdForeign])));
  assert(names[0][0] === 'B renamed' && names[1][0] === 'B renamed' && !names[0][1], 'after a lost New both pages follow the shared show: ' + JSON.stringify(names));

  // A refused edit is undone locally, and the next one goes through.
  await A.evaluate(() => edit(() => { tlObj(MAIN).duration_s = -5; }));
  await A.waitForTimeout(800);
  const after2 = await A.evaluate(() => [tlObj(MAIN).duration_s, S.pending.length]);
  assert(after2[0] > 0 && after2[1] === 0, 'a refused edit is reverted and not kept pending: ' + JSON.stringify(after2));
  await A.evaluate(() => edit(() => { S.doc.name = 'after refusal'; }));
  await A.waitForTimeout(800);
  assert(await B.evaluate(() => S.doc.name) === 'after refusal', 'the next edit after a refusal arrives');

  // An edit that arrived, but whose confirmation was lost with the
  // connection, is sent again on reconnect — and must not overwrite what
  // someone else changed in the meantime.
  await A.evaluate(() => {
    const ws = S.ws;
    const handler = ws.onmessage;
    ws.onmessage = (ev) => { if (JSON.parse(ev.data).type === 'show') { ws.onmessage = handler; ws.close(); } else handler(ev); };
    edit(() => { S.doc.name = 'alice'; }); sendDoc();
  });
  await A.waitForTimeout(300);
  await B.evaluate(() => edit(() => { S.doc.name = 'bob, later'; }));
  await A.waitForTimeout(2500);
  const n3 = await Promise.all([A, B].map((p) => p.evaluate(() => S.doc.name)));
  assert(n3[0] === 'bob, later' && n3[1] === 'bob, later', 'a re-sent edit does not overwrite a newer one: ' + JSON.stringify(n3));

  // Alice types into a cue's inspector while Bob moves that cue to another
  // track: her edit still lands on the cue.
  await A.evaluate(() => edit(() => {
    const o = tlObj(MAIN);
    o.tracks.push({ id: 'ta', name: 'TA', cues: [{ id: 'mc', name: 'm', source: { type: 'color', rgba: [1, 0, 0, 1] }, start_s: 0, duration_s: 5, in_s: 0, x: 0, y: 0, w: 10, h: 10, rotation: 0, opacity: 1, fade_in_s: 0, fade_out_s: 0 }] });
    o.tracks.push({ id: 'tb', name: 'TB', cues: [] });
  }));
  await A.waitForTimeout(800);
  await A.evaluate(() => select({ type: 'cue', id: 'mc' }));
  const nameInput = A.locator('#inspector input[type=text]').first();
  await nameInput.fill('renamed by alice');
  await B.evaluate(() => edit(() => { const o = tlObj(MAIN); const a = o.tracks.find((t) => t.id === 'ta'), b = o.tracks.find((t) => t.id === 'tb'); b.cues.push(a.cues.pop()); }));
  await A.waitForTimeout(800);
  await nameInput.press('Tab');
  await A.waitForTimeout(800);
  const moved = await B.evaluate(() => { const t = tlObj(MAIN).tracks.find((x) => x.id === 'tb'); return t.cues[0]?.name; });
  assert(moved === 'renamed by alice', 'an inspector edit reaches a cue someone else moved meanwhile: ' + moved);

  // Alice leaves: Bob stops showing her.
  await A.context().close(); await B.waitForTimeout(1500);
  assert(await B.evaluate(() => S.others.size) === 0, 'a user who leaves disappears');
  assert(!errs.length, 'no page errors ' + JSON.stringify(errs));
  await b.close();
})();
