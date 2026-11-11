// Pins and tags, saved in ccm's own state folder and read by both front ends.
import fs from "node:fs";
import { link, home, base, sleep, js, go, key, ok, names, count, finish } from "./lib.mjs";

const saved = `${home}/.local/state/ccm/labels.json`;
const file = () => { try { return JSON.parse(fs.readFileSync(saved, "utf8")); } catch { return null; } };
const status = () => js(`document.getElementById('status-text').textContent`);
const rowId = (text) => js(`[...document.querySelectorAll('#rows tr[data-id]')].find(t=>t.textContent.includes(${JSON.stringify(text)})).dataset.id`);
const pick = async (text) => { await js(`[...document.querySelectorAll('#rows tr[data-id]')].find(t=>t.textContent.includes(${JSON.stringify(text)})).click()`); await sleep(450); };
const pinnedNames = () => js(`[...document.querySelectorAll('#rows tr[data-id]')].filter(t=>t.querySelector('.pin')).map(t=>t.querySelector('.col-name').textContent)`);
const noReply = () => js(`fetch('/api/cleanup').then(r=>r.json()).then(d=>d.lines.find(l=>l.key==='no_reply').count)`);
const dialogOpen = () => js(`document.getElementById('dialog').open`);
const typeTags = async (text) => { await js(`document.querySelector('#dialog-body input').value = ${JSON.stringify(text)}`); await js(`document.querySelector('#dialog-buttons .main').click()`); await sleep(600); };

await go(link); await sleep(1500);
ok("nothing is pinned to start with", (await pinnedNames()).length === 0 && file() === null, await pinnedNames());
const junkBefore = await noReply();

await pick("Never answered");
const junkId = await rowId("Never answered");
await key("p", "KeyP"); await sleep(700);
ok("p pins it", (await pinnedNames()).length === 1, await pinnedNames());
ok("it moves to the top", (await names())[0] === "Never answered", await names());
ok("it says so", (await status()).includes("Pinned"), await status());
ok("it's saved in ccm's own file", file()?.pinned?.includes(junkId), file());
ok("the details offer to unpin it", await js(`[...document.querySelectorAll('#details button')].some(b=>b.textContent.startsWith('Unpin'))`));
await key("r", "KeyR"); await sleep(600);
ok("reversing the sort keeps it on top", (await names())[0] === "Never answered", await names());
await key("r", "KeyR"); await sleep(600);
ok("clean-up no longer suggests it", (await noReply()) === junkBefore - 1, [junkBefore, await noReply()]);

await key("d", "KeyD"); await sleep(700);
ok("it can't be deleted", !(await dialogOpen()), await status());
ok("and the page says why", (await status()).includes("pinned, so unpin it first"), await status());

await key("T", "KeyT", "T"); await sleep(400);
ok("T asks for tags", await dialogOpen());
await typeTags("Work, bug-1");
ok("the tags are saved", JSON.stringify(file()?.tags?.[junkId]) === JSON.stringify(["work", "bug-1"]), file());
ok("they show on the row", await js(`[...document.querySelectorAll('#rows tr[data-id] .label')].map(b=>b.textContent).join(' ')`) === "#work #bug-1");
ok("and in the details", await js(`document.getElementById('details').textContent.includes('#work #bug-1')`));

await js(`document.querySelector('#rows .label').click()`); await sleep(700);
ok("clicking a tag searches for it", await js(`document.getElementById('search').value`) === "#work");
ok("and shows only those", (await count()) === 1, await names());
await js(`document.getElementById('search').value = '#work #nope'; document.getElementById('search').dispatchEvent(new Event('input'))`); await sleep(700);
ok("every tag searched for must be there", (await count()) === 0);
await js(`document.getElementById('search').value = ''; document.getElementById('search').dispatchEvent(new Event('input'))`); await sleep(700);

await pick("Never answered");
await key("T", "KeyT", "T"); await sleep(400);
ok("the box starts with its tags", await js(`document.querySelector('#dialog-body input').value`) === "#work #bug-1 ");
await typeTags("no!");
ok("a bad tag keeps the box open", await dialogOpen());
ok("and says why", await js(`document.querySelector('#dialog-body .problem').textContent.includes("can't be a tag")`));
await key("Escape", "Escape"); await sleep(400);
ok("nothing changed", JSON.stringify(file()?.tags?.[junkId]) === JSON.stringify(["work", "bug-1"]), file());

// Marks two, one of them tagged, so they share no tags.
await pick("Never answered"); await key(" ", "Space");
await pick("Fix the checkout total"); await key(" ", "Space");
await key("T", "KeyT", "T"); await sleep(400);
ok("several start empty when they share nothing", await js(`document.querySelector('#dialog-body input').value`) === "");
ok("and the box says so", await js(`document.getElementById('dialog-title').textContent`) === "Tags for 2 conversations");
await typeTags("shared");
const fixId = await rowId("Fix the checkout total");
ok("the new tag goes on both and the old ones stay", JSON.stringify(file()?.tags?.[junkId]) === JSON.stringify(["work", "bug-1", "shared"]) && JSON.stringify(file()?.tags?.[fixId]) === JSON.stringify(["shared"]), file());
await key("Escape", "Escape"); await sleep(300);

// The terminal app saves to the same file.
const now = file();
now.pinned.push(await rowId("Huge old conversation"));
fs.writeFileSync(saved, JSON.stringify(now));
await sleep(3500);
ok("a pin made elsewhere shows on the next refresh", (await pinnedNames()).length === 2, await pinnedNames());

// Marking all and then none clears the two marks.
await key("a", "KeyA"); await key("a", "KeyA");
await pick("Never answered");
await key("p", "KeyP"); await sleep(700);
ok("p again unpins it", !(await pinnedNames()).some((n) => n.includes("Never answered")), await pinnedNames());
ok("and it's saved", !file().pinned.includes(junkId), file());

await go(base + "/"); await sleep(1200);
ok("pins and tags are still there after a reload", (await pinnedNames()).length === 1 && (await js(`document.querySelectorAll('#rows .label').length`)) === 4, [await pinnedNames(), await js(`document.querySelectorAll('#rows .label').length`)]);

await finish(["favicon"]);
