// The list: search, filters, sorting, grouping, the preview and hostile text.
import fs from "node:fs";
import http from "node:http";
import { link, out, home, project, base, sleep, send, js, go, shot, key, ok, names, count, finish, problems } from "./lib.mjs";

await go(link);
ok("link signs in and leaves the secret out of the address", (await js("location.href")) === base + "/", await js("location.href"));
await sleep(1500);
ok("all invented conversations listed", (await count()) === 8, await names());
ok("tool column and chip appear with two tools", await js(`!!document.querySelector('th.col-tool') && !document.getElementById('tool').hidden`));
await shot("1-list-light");
const all = await names();
ok("newest first by default", all[0] === "Fix the checkout total", all);
ok("markup in a name is shown as text", all.some((n) => n.includes("<img src=x onerror")), all);
ok("control characters are gone", all.some((n) => n.startsWith("evil hidden")), all);

// The hostile conversation.
await js(`[...document.querySelectorAll('#rows tr[data-id]')].find(t=>t.textContent.includes('<img src=x')).click()`); await sleep(500);
ok("hostile text creates no elements", (await js(`document.querySelectorAll('.preview img,.preview script,.preview b,#rows img,#rows script,.details img').length`)) === 0);
ok("hostile text ran nothing", (await js(`window.pwned===undefined`)) === true, await js(`window.pwned`));
ok("preview shows it literally", (await js(`document.getElementById('preview').textContent`)).includes("<b>bold?</b>"));
await shot("2-hostile");

// Selection, details, preview.
await js(`document.querySelector('#rows tr[data-id]').click()`); await sleep(500);
ok("details follow the selection", (await js(`document.querySelector('.details h2').textContent`)) === "Fix the checkout total");
const atEnd = await js(`(()=>{const p=document.getElementById('preview');return p.scrollHeight-p.scrollTop-p.clientHeight})()`);
ok("preview starts at the newest message", atEnd < 2, atEnd);
ok("tool calls are counted", (await js(`document.querySelector('.turn:not(.you) h3').textContent`)).includes("1 tool call"));
await key("j", "KeyJ"); await sleep(300);
ok("j moves down", (await js(`document.querySelector('tr.selected .col-name').firstChild.textContent`)) === all[1]);
await key("k", "KeyK"); await sleep(300);
ok("k moves up", (await js(`document.querySelector('tr.selected .col-name').firstChild.textContent`)) === all[0]);
await key("[", "BracketLeft");
ok("[ scrolls the preview back", (await js(`(()=>{const p=document.getElementById('preview');return p.scrollHeight-p.scrollTop-p.clientHeight})()`)) > 10);

// Search.
await key("/", "Slash");
ok("/ focuses search", await js(`document.activeElement.id==='search'`));
for (const c of "zebra") await key(c, "Key" + c.toUpperCase());
await sleep(500);
ok("search finds a word said in one conversation", JSON.stringify(await names()) === '["Feature branch work"]', await names());
await shot("3-search");
await key("Escape", "Escape"); await sleep(500);
ok("Esc clears search", (await count()) === 8);

// Filters.
const filter = async (k, code, want, label) => { await key(k, code); await sleep(350); const n = await names(); ok(label, JSON.stringify(n) === JSON.stringify(want), n); await key(k, code); await sleep(350); };
await filter("J", "KeyJ", ["Never answered"], "junk filter");
await filter("l", "KeyL", ["Huge old conversation"], "large filter");
await filter("m", "KeyM", ["Folder was removed"], "missing filter");
await key("t", "KeyT"); await sleep(350);
ok("tool chip names the tool", (await js(`document.getElementById('tool').textContent`)).includes("Tool: Claude"));
ok("tool filter: Claude", (await count()) === 7, await count());
await key("t", "KeyT"); await sleep(350);
ok("tool filter: Codex", (await count()) === 1, await names());
await js(`document.querySelector('#rows tr[data-id]').click()`); await sleep(500);
ok("Codex preview shows who replied", (await js(`document.querySelector('.turn:not(.you) h3').textContent`)).startsWith("Codex"));
await key("c", "KeyC"); await sleep(350);
ok("c clears filters", (await count()) === 8);

// Sorting.
await js(`document.querySelector('th.col-size button').click()`); await sleep(350);
ok("clicking a column sorts by it, biggest first", (await names())[0] === "Huge old conversation", await names());
ok("sort is announced", (await js(`document.querySelector('th.col-size').getAttribute('aria-sort')`)) === "descending");
await key("r", "KeyR"); await sleep(350);
ok("r reverses", (await names()).at(-1) === "Huge old conversation", await names());
await js(`document.querySelector('th.col-active button').click()`); await sleep(350);

// Grouping.
await key("g", "KeyG"); await sleep(400);
const headers = () => js(`[...document.querySelectorAll('tr.group td')].map(t=>t.firstChild.textContent)`);
ok("group by folder", (await headers()).includes("~/notes"), await headers());
await key("g", "KeyG"); await sleep(2500);
const h2 = await headers();
ok("group by repository joins the worktree", h2.includes("~/work/shop") && !h2.includes("~/work/shop-feature"), h2);
ok("not in Git is said", h2.some((x) => x === "~/notes (not in Git)"), h2);
ok("repository rows carry a tag", (await js(`document.querySelector('.tag')?.textContent||''`)).length > 0);
await shot("4-group-repo");
const widths = await js(`(()=>{const l=document.getElementById('list').clientWidth,t=document.querySelector('table').offsetWidth,n=document.querySelector('th.col-name').offsetWidth;return [l,t,n]})()`);
ok("grouped table fills the pane and leaves room for names", widths[1] === widths[0] && widths[2] > 200, widths);
await key("g", "KeyG"); await sleep(400);

// Live updates.
await js(`document.querySelector('th.col-active button').click()`); await sleep(200);
const sid = "11111111-2222-4333-8444-555555555555";
fs.mkdirSync(project, { recursive: true });
const t = new Date().toISOString();
fs.writeFileSync(`${project}/${sid}.jsonl`, [JSON.stringify({ type: "custom-title", customTitle: "Appeared while watching" }), JSON.stringify({ type: "user", cwd: home + "/notes", timestamp: t, message: { role: "user", content: "new" } }), JSON.stringify({ type: "assistant", cwd: home + "/notes", timestamp: t, message: { id: "z", model: "claude-test-5", content: [{ type: "text", text: "yes" }] } })].join("\n") + "\n");
let seen = false; for (let i = 0; i < 40 && !seen; i++) { await sleep(250); seen = (await names()).includes("Appeared while watching"); }
ok("a new conversation appears by itself", seen);
fs.unlinkSync(`${project}/${sid}.jsonl`);
let gone = false; for (let i = 0; i < 40 && !gone; i++) { await sleep(250); gone = !(await names()).includes("Appeared while watching"); }
ok("a removed one goes by itself", gone);

// A list that isn't changing isn't redrawn.
await js(`window.mark=document.querySelector('#rows tr[data-id]')`); await sleep(4500);
ok("a quiet list is left alone", await js(`window.mark===document.querySelector('#rows tr[data-id]')`));

// Attacks from a real browser: another port is the closest a website can get.
const attacker = http.createServer((_, res) => { res.setHeader("Content-Type", "text/html"); res.end("<!doctype html><p>evil</p>"); }).listen(0, "127.0.0.1");
await sleep(100);
await go(`http://127.0.0.1:${attacker.address().port}/`);
const tryIt = (mode) => js(`fetch(${JSON.stringify(base + "/api/list")},{mode:${JSON.stringify(mode)},credentials:'include'}).then(async r=>r.type+':'+r.status+':'+(await r.text()).length).catch(e=>'blocked')`);
const cors = await tryIt("cors"); ok("another website can't read the list", cors === "blocked", cors);
const opaque = await tryIt("no-cors"); ok("a no-cors request gets nothing back", opaque === "opaque:0:0" || opaque === "blocked", opaque);
attacker.close();
problems.length = 0; // The browser logs the blocked attack as an error.

// Other widths and the dark theme.
await go(base + "/"); await sleep(800);
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
await sleep(200); await shot("5-list-dark");
await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 800, deviceScaleFactor: 2, mobile: true }); await sleep(500);
ok("no sideways scroll on a phone", (await js(`document.documentElement.scrollWidth<=window.innerWidth`)) === true, await js(`[document.documentElement.scrollWidth,innerWidth]`));
await shot("6-phone");
await send("Emulation.setDeviceMetricsOverride", { width: 1000, height: 700, deviceScaleFactor: 1, mobile: false }); await sleep(500);
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "light" }] });
await shot("7-narrow");

await go(base + "/"); await sleep(1000);
ok("where focus can't be controlled the box says so", (await js(`document.getElementById('focus-chip').title`)).includes("can't control focus"));
await js(`document.getElementById('focus-new').click()`); await sleep(400);
ok("and ticking it explains instead of working", (await js(`document.getElementById('status-text').textContent`)).includes("can't control focus") && !(await js(`document.getElementById('focus-new').checked`)));

await finish(["403", "favicon", "blocked by CORS"]);
