// Deleting conversations: the popup, its choices and what stays.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import { link, out, home, project, termLog, base, sleep, send, js, go, shot, key, ok, names, count, finish } from "./lib.mjs";

await go(link); await sleep(1500);
await send("Browser.grantPermissions", { origin: base, permissions: ["clipboardReadWrite", "clipboardSanitizedWrite"] });
const pick = async (text) => { await js(`[...document.querySelectorAll('#rows tr[data-id]')].find(t=>t.textContent.includes(${JSON.stringify(text)})).click()`); await sleep(450); };
const log = () => { try { return fs.readFileSync(termLog, "utf8").trim().split("\n").filter(Boolean); } catch { return []; } };
const status = () => js(`document.getElementById('status').hidden?'':document.getElementById('status-text').textContent`);
const dialogOpen = () => js(`document.getElementById('dialog').open`);
const can = (n) => js(`[...document.querySelectorAll('.actions button')].find(b=>b.textContent.startsWith(${JSON.stringify(n)})).disabled===false`);
const selId = () => js(`document.querySelector('tr.selected').dataset.id`);


const row = (t) => `[...document.querySelectorAll('#rows tr[data-id]')].find(r=>r.textContent.includes(${JSON.stringify(t)}))`;
const dbody = () => js(`document.getElementById('dialog-body').textContent`);
const gitwt = () => execFileSync("git", ["-C", home + "/work/shop", "worktree", "list", "--porcelain"], { env: { ...process.env, GIT_OPTIONAL_LOCKS: "0" } }).toString();
const hasRow = async (t) => (await names()).includes(t);
const transcript = async (name) => { const found = []; const walk = (d) => { for (const e of fs.readdirSync(d, { withFileTypes: true })) { const p = d + "/" + e.name; if (e.isDirectory()) walk(p); else if (p.endsWith(".jsonl") && fs.readFileSync(p, "utf8").includes(`"customTitle": "${name}"`)) found.push(p); } }; walk(home + "/.claude/projects"); return found; };
const settle = async () => { for (let i = 0; i < 150; i++) { if (!(await js(`planning`))) break; await sleep(100); } await sleep(150); };
const total0 = await count();

// Cancel changes nothing.
await pick("Fix the checkout total");
ok("Delete button offered", await can("Delete"));
await key("d", "KeyD"); await settle();
ok("d opens the popup", await dialogOpen());
ok("popup lists what goes, with a short id", (await dbody()).includes("Fix the checkout total") && (await dbody()).includes("can't be undone"), await dbody());
ok("Cancel has the focus", await js(`document.activeElement.textContent==='Cancel'`));
await shot("30-delete");
await key("Enter", "Enter"); await sleep(500);
ok("Enter on Cancel deletes nothing", !(await dialogOpen()) && (await count()) === total0 && (await status()) === "Cancelled. Nothing was deleted.", await status());
await key("d", "KeyD"); await settle();
await key("Escape", "Escape"); await sleep(500);
ok("Esc cancels too", !(await dialogOpen()) && (await count()) === total0 && (await status()) === "Cancelled. Nothing was deleted.", await status());

// Plain delete.
await pick("Never answered");
const never = await transcript("Never answered");
ok("found the file to delete", never.length === 1, never);
await key("d", "KeyD"); await settle();
await key("y", "KeyY"); await sleep(900);
ok("y deletes the file", !fs.existsSync(never[0]));
ok("it leaves the list", !(await hasRow("Never answered")) && (await count()) === total0 - 1, await names());
ok("and says how much", (await status()).startsWith("Deleted 1 conversation, freed"), await status());
await key("/", "Slash"); for (const c of "hello") await key(c, "Key" + c.toUpperCase()); await sleep(500);
ok("its words are out of the search", (await count()) === 0, await names());
await key("Escape", "Escape"); await sleep(400);

// Del pause.
await pick("Folder was removed");
await key("Delete", "Delete"); await settle();
await key("Delete", "Delete"); await sleep(300);
ok("Del too soon is refused", (await dialogOpen()) && (await js(`document.querySelector('.foot').textContent`)).startsWith("Too quick"), await js(`document.querySelector('.foot').textContent`));
await sleep(1100);
await key("Delete", "Delete"); await sleep(800);
ok("Del after a pause deletes", !(await dialogOpen()) && !(await hasRow("Folder was removed")), await names());

// A double click on Delete can't confirm.
await pick("evil hidden");
await js(`(()=>{document.getElementById('delete-button').click();window.__clicked=new Promise(done=>{const t=setInterval(()=>{const b=document.querySelector('#dialog-buttons .danger');if(document.getElementById('dialog').open&&b){clearInterval(t);b.click();done();}},5);});})()`);
await js(`window.__clicked`); await sleep(400);
ok("a click right after opening isn't an answer", (await dialogOpen()) && (await hasRow("evil hidden name")), await dbody());
await js(`[...document.querySelectorAll('#dialog-buttons button')].find(b=>b.textContent==='Cancel').click()`); await sleep(300);

// Notes.
await pick("Wrote a memory note");
const noteFile = project + "/memory/fact.md";
await key("d", "KeyD"); await settle();
ok("the note is listed and kept by default", (await dbody()).includes("fact.md") && (await dbody()).includes("will be kept"), await dbody());
await key("Escape", "Escape"); await sleep(300);
await key("d", "KeyD"); await settle();
await key("m", "KeyM"); await sleep(200);
ok("m says it will go", (await dbody()).includes("will be deleted"), await dbody());
await shot("31-notes");
await key("y", "KeyY"); await sleep(900);
let noteGone=false; for(let i=0;i<80&&!noteGone;i++){noteGone=!fs.existsSync(noteFile); if(!noteGone) await sleep(100);}
ok("the note goes when asked", noteGone);

// A worktree that's clean.
await pick("Only one in the clean worktree");
await key("d", "KeyD"); await settle();
ok("its worktree is offered, kept by default", (await dbody()).includes("~/work/shop-clean") && (await dbody()).includes("are kept unless"), await dbody());
await key("w", "KeyW"); await sleep(500);
ok("w says it will go", (await dbody()).includes("will be removed too"), await dbody());
ok("no countdown without unpushed commits", await js(`!document.querySelector('#dialog-buttons .danger').disabled`));
await key("y", "KeyY"); await sleep(1500);
ok("worktree removed, conversation deleted", !gitwt().includes("shop-clean") && !fs.existsSync(home + "/work/shop-clean") && !(await hasRow("Only one in the clean worktree")), gitwt());
ok("says the branch stays", (await status()).includes("Its branch is kept."), await status());
ok("the branch really stays", execFileSync("git", ["-C", home + "/work/shop", "branch", "--list", "clean"]).toString().includes("clean"));

// Unpushed commits.
await pick("Only one in the ahead worktree");
await key("d", "KeyD"); await settle();
ok("unpushed commits are warned about", (await dbody()).includes("No remote, so 1 commit is only on this machine."), await dbody());
ok("Delete works before asking for the worktree", await js(`!document.querySelector('#dialog-buttons .danger').disabled`));
await key("w", "KeyW"); await sleep(400);
ok("asking for it locks Delete", await js(`document.querySelector('#dialog-buttons .danger').disabled`));
ok("and says how long", (await js(`document.querySelector('.foot').textContent`)).startsWith("Delete unlocks in"), await js(`document.querySelector('.foot').textContent`));
await shot("32-countdown");
await sleep(1500);
await key("y", "KeyY"); await sleep(400);
ok("y too soon starts it over", (await js(`document.querySelector('.foot').textContent`)).startsWith("Too early, so it started over. Delete unlocks in 5s") || (await js(`document.querySelector('.foot').textContent`)).startsWith("Too early, so it started over. Delete unlocks in 4s"), await js(`document.querySelector('.foot').textContent`));
ok("nothing went", gitwt().includes("shop-ahead") && (await hasRow("Only one in the ahead worktree")));
// A changed page can't skip the wait.
const forced = await js(`(async()=>{const d=await (await fetch('/api/delete/plan',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({ids:[selected]})})).json();const t=await (await fetch('/api/delete/worktrees',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({plan_id:d.plan_id,include:true})})).json();const c=await (await fetch('/api/delete/confirm',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({plan_id:d.plan_id,worktrees:true})})).json();window.freshPlan=d.plan_id;return c;})()`);
ok("the server holds the wait, whatever the page does", forced.restarted === true && gitwt().includes("shop-ahead"), forced);
await js(`[...document.querySelectorAll('#dialog-buttons button')].find(b=>b.textContent==='Cancel').click()`); await sleep(300);
ok("the replaced popup's plan is cancelled quietly", !(await dialogOpen()));
await key("d", "KeyD"); await settle();
await key("w", "KeyW"); await sleep(5600);
ok("unlocks after the wait", await js(`!document.querySelector('#dialog-buttons .danger').disabled`), await js(`document.querySelector('.foot').textContent`));
await sleep(300);
await key("y", "KeyY"); await sleep(1500);
ok("worktree goes after the wait, its commit kept on its branch", !gitwt().includes("shop-ahead") && execFileSync("git", ["-C", home + "/work/shop", "log", "--oneline", "ahead"]).toString().includes("two"), gitwt());

// Marks, and what a filter hides.
const m0 = await count();
await js(`${row("Feature branch work")}.querySelector('.col-mark input').click()`); await sleep(150);
await js(`${row("Huge old")}.querySelector('.col-mark input').click()`); await sleep(150);
ok("marks count and size show", (await js(`document.getElementById('summary').textContent`)).includes("2 marked"), await js(`document.getElementById('summary').textContent`));
ok("Delete button counts them", await js(`document.getElementById('delete-button').firstChild.textContent==='Delete 2 '`));
await shot("33-marks");
await key("l", "KeyL"); await sleep(400);
ok("a filter hides one of the marks from the count", (await js(`document.getElementById('summary').textContent`)).includes("1 marked"), await js(`document.getElementById('summary').textContent`));
await key("d", "KeyD"); await settle();
ok("a delete can't reach a hidden mark", (await dbody()).includes("Huge old") && !(await dbody()).includes("Feature branch work"), await dbody());
await key("Escape", "Escape"); await sleep(300);
await key("l", "KeyL"); await sleep(400);
await key("a", "KeyA"); await sleep(200);
ok("a marks everything", (await js(`document.querySelectorAll('tr.marked').length`)) === m0);
await key("a", "KeyA"); await sleep(200);
ok("a again clears them", (await js(`document.querySelectorAll('tr.marked').length`)) === 0);
await pick("Fix the checkout total");
await key(" ", "Space"); await sleep(250);
ok("Space marks and moves down", (await js(`document.querySelector('tr.marked .col-name').firstChild.textContent`)) === "Fix the checkout total" && (await js(`document.querySelector('tr.selected .col-name').firstChild.textContent`)) !== "Fix the checkout total");
await js(`marked.clear(); drawMarks();`); await sleep(100);

// Codex needs its own command, which isn't installed here.
await pick("Fix the failing parser test");
await key("d", "KeyD"); await settle();
ok("no codex command means no delete, and says why", !(await dialogOpen()) && /codex/i.test(await status()), await status());

// Several, with one left by the server.
await js(`document.querySelectorAll('tr.marked').forEach(t=>t.querySelector('.col-mark input').click())`); await sleep(100);
await js(`${row("Fix the failing parser test")}.querySelector('.col-mark input').click()`);
await js(`${row("Fix the checkout total")}.querySelector('.col-mark input').click()`); await sleep(150);
await key("d", "KeyD"); await settle();
ok("a mixed delete names what's skipped", /Skipping Fix the failing parser test: /.test(await dbody()) && (await dbody()).includes("1 conversation"), await dbody());
await key("y", "KeyY"); await sleep(1000);
ok("the rest goes and a report says what stayed", (await dialogOpen()) && (await dbody()).includes("Skipped Fix the failing parser test"), await dbody());
await shot("34-report");
await js(`document.querySelector('#dialog-buttons .main').click()`); await sleep(400);
ok("the deleted one's mark is gone too", (await js(`document.getElementById('summary').textContent`)).includes("1 marked"), await js(`document.getElementById('summary').textContent`));

// From another website.
const before = await count();
const victim = await js(`document.querySelector("tr[data-id]").dataset.id`);
const attacker = http.createServer((_, res) => { res.setHeader("Content-Type", "text/html"); res.end("<!doctype html><p>evil</p>"); }).listen(0, "127.0.0.1");
await sleep(100);
await go(`http://127.0.0.1:${attacker.address().port}/`);
await js(`fetch(${JSON.stringify(base + "/api/delete/plan")},{method:'POST',mode:'no-cors',credentials:'include',headers:{'Content-Type':'text/plain'},body:JSON.stringify({ids:[${JSON.stringify(victim)}]})}).catch(()=>0)`);
await sleep(300); attacker.close();
await go(base + "/"); await sleep(900);
ok("another website can't delete", (await count()) === before);

await finish(["403", "blocked by CORS", "ERR_FAILED", "ERR_BLOCKED", "favicon"]);
