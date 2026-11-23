// Folders in the Assets window, in a real browser: a folder picked with
// "Upload folder…" is uploaded with the folders it contains, made again in
// the show; files that are not media are skipped; uploading it again makes
// no second folder; "+ Folder" makes one and names it in place; assets and
// folders are filed by dragging; a folder cannot go into its own subfolder;
// removing a folder moves what it held up a level; a dropped folder is read
// through the File and Directory Entries API (driven here with stand-in
// entries, since a test cannot drop real folders); another Producer sees the
// same folders; and nothing of this moves a file on the Asset Manager.
//
// Needs Node with Playwright, ffmpeg, and an Asset Manager, a Director and a
// Producer running on this machine, the Asset Manager on an EMPTY media
// folder that this script can read:
//
//   LCMS_BIND=127.0.0.1:7403 LCMS_MEDIA_DIR=/tmp/lcplay-fold/media target/debug/lc-assets &
//   target/debug/lc-director --state-dir /tmp/lcplay-fold/dir &
//   target/debug/lc-producer --browser &
//   node crates/lc-producer/ui-tests/folders.js /tmp/lcplay-fold/media [screenshot.png]
//
// Exit code 1 on any failure.
const { chromium } = require('playwright');
const { execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const assert = (c, m) => { if (!c) { console.log('FAIL', m); process.exitCode = 1; } else console.log('ok  ', m); };

const media = process.argv[2];
if (!media) { console.log('usage: node folders.js <Asset Manager media folder> [screenshot.png]'); process.exit(2); }

// A show folder as an operator might have it:
//   Show A/intro.mp4, Show A/logo.png, Show A/notes.txt (not media),
//   Show A/Loops/loop.mp4, Show A/.cache/junk.png (hidden folder)
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'lcplay-folders-'));
const showA = path.join(root, 'Show A');
for (const d of ['', 'Loops', '.cache']) fs.mkdirSync(path.join(showA, d), { recursive: true });
const ff = (args, out) => execFileSync('ffmpeg', ['-loglevel', 'error', '-y', '-f', 'lavfi', ...args, out]);
ff(['-i', 'testsrc2=size=320x180:rate=25', '-t', '1', '-c:v', 'libx264'], path.join(showA, 'intro.mp4'));
ff(['-i', 'testsrc2=size=320x180:rate=25', '-t', '1', '-c:v', 'libx264'], path.join(showA, 'Loops', 'loop.mp4'));
ff(['-i', 'color=c=red:size=64x64', '-frames:v', '1'], path.join(showA, 'logo.png'));
ff(['-i', 'color=c=blue:size=64x64', '-frames:v', '1'], path.join(showA, '.cache', 'junk.png'));
fs.writeFileSync(path.join(showA, 'notes.txt'), 'not media\n');

(async () => {
  const br = await chromium.launch();
  const p = await br.newPage({ viewport: { width: 1600, height: 1000 } });
  const errs = [];
  p.on('pageerror', (e) => errs.push(e.message));
  await p.goto('http://127.0.0.1:7404');
  await p.waitForFunction(() => Array.isArray(S.library) && S.doc);
  // A fresh show, so folders of an earlier run do not count.
  await p.evaluate(() => edit(() => { S.doc.asset_folders = []; S.doc.asset_placement = {}; }));

  const folders = () => p.evaluate(() => S.doc.asset_folders.map((f) => ({ id: f.id, name: f.name, parent: f.parent })));
  const byName = async (name) => (await folders()).find((f) => f.name === name);
  const placedIn = (file) => p.evaluate((file) => {
    const item = S.library.find((i) => i.name === file);
    return item ? folderPath(assetFolder(item.id)) : null;
  }, file);
  // The list as shown: "folder:Name" or the asset's name, indented by level.
  const shown = () => p.$$eval('#asset-list > .folder, #asset-list > .asset:not(.upload)', (rows) => rows.map((r) => {
    const depth = Math.round((parseFloat(r.style.marginLeft) || 0) / 14);
    return '  '.repeat(depth) + (r.classList.contains('folder') ? `folder:${r.querySelector('.name').textContent}` : r.querySelector('.name').textContent);
  }));
  const settle = async () => {
    await p.waitForFunction(() => S.uploads.length === 0, null, { timeout: 30000 });
    await p.waitForTimeout(400); // the edits reach the Director and come back
  };
  const lastToast = () => p.$eval('#toast', (t) => t.textContent);

  // ── Upload folder…: the folders come too; non-media and hidden are skipped.
  await p.setInputFiles('#folder-upload', showA);
  const skippedToast = await lastToast();
  const queued = await p.evaluate(() => S.uploads.map((u) => u.name).sort());
  assert(queued.join() === 'intro.mp4,logo.png,loop.mp4', `only the media are uploaded (${queued})`);
  assert(/1 file is not media .*notes\.txt/.test(skippedToast), `the skipped file is named (${skippedToast})`);
  const note = await p.$$eval('#asset-list .asset.upload .up-note', (n) => n.map((x) => x.textContent));
  assert(note.includes('into Show A') && note.includes('into Show A / Loops'), `upload rows say where they go (${note})`);
  await settle();
  let f = await folders();
  const a = f.find((x) => x.name === 'Show A'), loops = f.find((x) => x.name === 'Loops');
  assert(f.length === 2 && a && loops && a.parent === '' && loops.parent === a.id, `the show has Show A, and Loops inside it (${JSON.stringify(f)})`);
  assert(await placedIn('intro.mp4') === 'Show A' && await placedIn('logo.png') === 'Show A', 'intro.mp4 and logo.png are filed in Show A');
  assert(await placedIn('loop.mp4') === 'Show A / Loops', 'loop.mp4 is filed in Show A / Loops');
  const disk = fs.readdirSync(media).filter((n) => !n.startsWith('.')).sort();
  assert(disk.join() === 'intro.mp4,logo.png,loop.mp4', `on disk the files are side by side, nothing else (${disk})`);
  let list = await shown();
  assert(list.join('|') === 'folder:Show A|  folder:Loops|    loop.mp4|  intro.mp4|  logo.png', `the list shows the tree (${list.join(' | ')})`);
  const count = await p.$eval('#asset-list .folder .count', (c) => c.textContent);
  assert(count === '3', `a folder counts the assets in it and its subfolders (${count})`);

  // ── The same folder again: no second Show A.
  await p.setInputFiles('#folder-upload', showA);
  await settle();
  f = await folders();
  assert(f.length === 2, `uploading it again reuses the folders (${f.map((x) => x.name)})`);

  // ── + Folder: named in place.
  await p.click('#btn-new-folder');
  const input = p.locator('#asset-list .folder input.rename');
  assert(await input.count() === 1 && await input.inputValue() === 'New folder', 'a new folder asks for its name');
  await input.fill('Graphics');
  await input.press('Enter');
  await p.waitForTimeout(100);
  const gfx = await byName('Graphics');
  assert(gfx && gfx.parent === '', 'it is named Graphics, at the top level');
  // Escape keeps the old name.
  await p.dblclick('#asset-list .folder[data-folder="' + gfx.id + '"] .name');
  await input.fill('Nope');
  await input.press('Escape');
  assert((await byName('Graphics')) && !(await byName('Nope')), 'Escape leaves the name as it was');

  // ── Drag an asset onto a folder.
  const row = (name) => p.locator('#asset-list .asset:not(.upload)', { hasText: name });
  const folderRowOf = (id) => p.locator(`#asset-list .folder[data-folder="${id}"]`);
  await row('logo.png').dragTo(folderRowOf(gfx.id));
  await p.waitForTimeout(100);
  assert(await placedIn('logo.png') === 'Graphics', 'logo.png dragged onto Graphics is filed there');
  // Onto another asset: into that asset's folder.
  await row('intro.mp4').dragTo(row('loop.mp4'));
  await p.waitForTimeout(100);
  assert(await placedIn('intro.mp4') === 'Show A / Loops', 'dropped on an asset, it goes into that asset\'s folder');

  // ── Folders into folders; never into themselves.
  await folderRowOf(loops.id).dragTo(folderRowOf(gfx.id));
  await p.waitForTimeout(100);
  assert((await byName('Loops')).parent === gfx.id, 'Loops dragged onto Graphics goes inside it');
  await folderRowOf(gfx.id).dragTo(folderRowOf(loops.id));
  await p.waitForTimeout(100);
  assert((await byName('Graphics')).parent === '', 'Graphics cannot go into its own subfolder');
  // Undo puts Loops back in Show A.
  await p.keyboard.press('Control+z');
  await p.waitForTimeout(100);
  assert((await byName('Loops')).parent === a.id, 'undo puts Loops back in Show A');

  // ── Closing a folder hides what it holds (per computer).
  await folderRowOf(a.id).click({ position: { x: 8, y: 8 } });
  list = await shown();
  assert(list.join('|') === 'folder:Graphics|  logo.png|folder:Show A', `a closed folder hides its contents (${list.join(' | ')})`);
  await folderRowOf(a.id).click({ position: { x: 8, y: 8 } });

  // ── A dropped folder, read entry by entry. Stand-in entries with the
  // shape of FileSystemEntry; the reader gives one entry per batch, as a
  // large folder would in several.
  await p.evaluate(async (into) => {
    const fileEntry = (name, bytes) => ({ isFile: true, isDirectory: false, name, file: (ok) => ok(new File([bytes], name)) });
    const dirEntry = (name, children) => ({
      isFile: false, isDirectory: true, name,
      createReader: () => { let i = 0; return { readEntries: (ok) => ok(i < children.length ? [children[i++]] : []) }; },
    });
    const png = Uint8Array.from(atob('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=='), (c) => c.charCodeAt(0));
    const tree = dirEntry('Dropped', [fileEntry('b-still.png', png), dirEntry('Deep', [fileEntry('a-still.png', png), fileEntry('readme.md', 'x')])]);
    const dt = { items: [{ kind: 'file', webkitGetAsEntry: () => tree }], files: [] };
    await uploadDropped(dt, into);
  }, gfx.id);
  await settle();
  f = await folders();
  const dropped = f.find((x) => x.name === 'Dropped'), deep = f.find((x) => x.name === 'Deep');
  assert(dropped?.parent === gfx.id && deep?.parent === dropped?.id, 'a folder dropped on Graphics is made inside it, with its subfolder');
  assert(await placedIn('b-still.png') === 'Graphics / Dropped' && await placedIn('a-still.png') === 'Graphics / Dropped / Deep', 'its files are filed in them');
  assert(!fs.existsSync(path.join(media, 'readme.md')), 'readme.md (not media) was not uploaded');

  // ── Another Producer sees the same folders.
  const p2 = await br.newPage({ viewport: { width: 1600, height: 1000 } });
  p2.on('pageerror', (e) => errs.push(e.message));
  await p2.goto('http://127.0.0.1:7404');
  await p2.waitForFunction(() => Array.isArray(S.library) && S.library.length && S.doc?.asset_folders?.length);
  const other = await p2.evaluate(() => S.library.map((i) => `${i.name}:${folderPath(assetFolder(i.id))}`).sort());
  assert(other.join() === 'a-still.png:Graphics / Dropped / Deep,b-still.png:Graphics / Dropped,intro.mp4:Show A / Loops,logo.png:Graphics,loop.mp4:Show A / Loops',
    `the other Producer has the same folders (${other})`);
  if (process.argv[3]) await p.screenshot({ path: process.argv[3], clip: await p.locator('#win-assets').boundingBox() });

  // ── Removing a folder: what it held moves up; no file is deleted.
  await folderRowOf(dropped.id).hover();
  await folderRowOf(dropped.id).locator('button.danger').click();
  await p.waitForTimeout(100);
  f = await folders();
  assert(!f.some((x) => x.name === 'Dropped') && (await byName('Deep')).parent === gfx.id, 'Dropped is gone; Deep moved up into Graphics');
  assert(await placedIn('b-still.png') === 'Graphics', 'b-still.png moved up into Graphics');
  assert(fs.existsSync(path.join(media, 'b-still.png')), 'and the file is still on the Asset Manager');
  // The other Producer follows.
  await p2.waitForFunction(() => !S.doc.asset_folders.some((x) => x.name === 'Dropped'));
  assert(true, 'the other Producer sees the folder removed');

  const after = fs.readdirSync(media).filter((n) => !n.startsWith('.')).sort();
  assert(after.join() === 'a-still.png,b-still.png,intro.mp4,logo.png,loop.mp4', `files on disk never moved (${after})`);
  assert(!errs.length, `no page errors (${errs.join('; ')})`);
  await br.close();
  fs.rmSync(root, { recursive: true, force: true });
})().catch((e) => { console.log('FAIL', e.message); process.exitCode = 1; process.exit(1); });
