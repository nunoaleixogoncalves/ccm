// Resume, fork, rename, reopen and copying, and requests from other websites.
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

// Resume.
await pick("Fix the checkout total"); const a = await selId();
ok("resume is offered", await can("Resume"));
await key("Enter", "Enter"); await sleep(700);
let l = log();
ok("Enter starts claude --resume in a terminal", l.length === 1 && l[0].includes(`claude --resume ${a}`) && l[0].includes("work/shop"), l);
ok("says where it opened", (await status()).startsWith("Opened in a new terminal"), await status());
await sleep(300);
ok("resume greys out while starting", !(await can("Resume")));
ok("rename greys out while starting", !(await can("Rename")));
await js(`resume()`).catch(()=>{}); 
await sleep(500);
ok("second resume is refused, nothing else started", log().length === 1 && (await status()).startsWith("Already starting"), [log(), await status()]);
await shot("20-resumed");

// A double click on another row.
await js(`(()=>{const t=[...document.querySelectorAll('#rows tr[data-id]')].find(t=>t.textContent.includes('Feature branch work'));t.click();t.dispatchEvent(new MouseEvent('dblclick',{bubbles:true}));})()`); await sleep(700);
ok("double click resumes once", log().length === 2 && log()[1].includes("shop-feature"), log());

// Enter on a button is the button's.
await js(`document.getElementById('group').focus()`); await key("Enter", "Enter"); await sleep(300);
ok("Enter on a focused button doesn't resume", log().length === 2 && (await js(`document.getElementById('group').textContent`)).includes("folder"), log());
await js(`document.getElementById('group').blur(); nextGroup(); nextGroup();`); await sleep(500);

// Codex resume.
await pick("Fix the failing parser test"); const cx = await selId();
await key("Enter", "Enter"); await sleep(700);
l = log(); ok("Codex resumes with its own command", l.length === 3 && l[2].includes(`codex resume -C`) && l[2].includes(cx), l);

// Fork.
await pick("evil hidden"); const f0 = await selId();
await key("f", "KeyF"); await sleep(700);
l = log(); const forkLine = l[3] || "";
ok("fork starts a copy with a new id", forkLine.includes(`--resume ${f0}`) && forkLine.includes("--fork-session"), l);
ok("fork says it will appear", (await status()).includes("shows here once Claude Code saves it"), await status());
const newId = /--session-id ([0-9a-f-]+)/.exec(forkLine)?.[1];
const t = new Date().toISOString();
fs.writeFileSync(`${project}/${newId}.jsonl`, [JSON.stringify({ type: "custom-title", customTitle: "The new fork" }), JSON.stringify({ type: "user", cwd: home + "/notes", timestamp: t, message: { role: "user", content: "new" } }), JSON.stringify({ type: "assistant", cwd: home + "/notes", timestamp: t, message: { id: "z", model: "claude-test-5", content: [{ type: "text", text: "yes" }] } })].join("\n") + "\n");
let sel = false; for (let i = 0; i < 40 && !sel; i++) { await sleep(250); sel = (await selId()) === newId; }
ok("the fork is selected once saved", sel);
await sleep(500);
ok("the fork says what it came from", (await js(`document.querySelector('.notes')?.textContent||''`)).startsWith("Forked from evil hidden"), await js(`document.querySelector('.notes')?.textContent||''`));

// Missing folder.
await pick("Folder was removed");
const gone = home + "/deleted-project";
await key("Enter", "Enter"); await sleep(500);
ok("a missing folder asks first", await dialogOpen());
ok("nothing made yet", !fs.existsSync(gone) && log().length === 4, log());
await shot("21-folder");
await key("j", "KeyJ"); await sleep(150);
ok("keys don't act behind a dialog", await dialogOpen());
ok("Cancel has the focus", await js(`document.activeElement.textContent==='Cancel'`));
await key("Enter", "Enter"); await sleep(400);
ok("Enter on Cancel makes nothing", !(await dialogOpen()) && !fs.existsSync(gone) && log().length === 4 && (await status()) === "Cancelled.", [log(), await status()]);
await key("Enter", "Enter"); await sleep(500);
await js(`document.querySelector('#dialog-buttons .main').click()`); await sleep(800);
ok("Recreate makes it and resumes", fs.existsSync(gone) && log().length === 5 && log()[4].includes("deleted-project"), log());

// Failure gives the command.
await pick("Never answered");
fs.writeFileSync(out + "/fail", "");
await key("Enter", "Enter"); await sleep(1200);
fs.unlinkSync(out + "/fail");
ok("a failed start says so", (await status()).startsWith("Couldn't start it"), await status());
ok("and offers the command", await js(`!document.getElementById('status-copy').hidden`));
await shot("22-failed");
await js(`document.getElementById('status-copy').click()`); await sleep(300);
ok("the command is copied", (await js(`navigator.clipboard.readText()`)).includes("--resume"), await js(`navigator.clipboard.readText()`));
await key("Enter", "Enter"); await sleep(800);
ok("it can be tried again after failing", (await status()).startsWith("Opened"), await status());

// Copy.
await pick("Huge old"); const h = await selId();
await key("y", "KeyY"); await sleep(300);
ok("y copies the ID", (await js(`navigator.clipboard.readText()`)) === h);
await key("Y", "KeyY"); await sleep(300);
ok("Y copies the command", (await js(`navigator.clipboard.readText()`)).includes(`claude --resume ${h}`), await js(`navigator.clipboard.readText()`));

// Rename.
await pick("Huge old");
await key("R", "KeyR"); await sleep(400);
ok("rename opens with the name", await js(`document.querySelector('#dialog-body input').value==='Huge old conversation'`));
await js(`document.querySelector('#dialog-body input').value='<b>Renamed</b> & f'`);
await shot("23-rename");
await key("Enter", "Enter"); await sleep(800);
ok("renamed in the list", (await names()).includes("<b>Renamed</b> & f"), await names());
ok("and says so", (await status()) === "Renamed to <b>Renamed</b> & f.", await status());
const files = fs.readdirSync(project); const trans = fs.readFileSync(`${project}/${h}.jsonl`, "utf8");
ok("written where Claude Code reads it", trans.includes('"customTitle":"<b>Renamed</b> & f"'));
ok("no fork or resume came from typing", log().length === 6, log().length);
await key("R", "KeyR"); await sleep(300);
await js(`document.querySelector('#dialog-body input').value='   '`);
await key("Enter", "Enter"); await sleep(500);
ok("an empty name is refused", (await status()) === "The name is empty.", await status());

await pick("Fix the failing parser test");
for (let i = 0; i < 100 && !(await can("Rename")); i++) await sleep(300);
await key("R", "KeyR"); await sleep(300);
await js(`document.querySelector('#dialog-body input').value='Codex renamed here'`);
await key("Enter", "Enter"); await sleep(800);
ok("a Codex conversation is renamed in Codex's own index", fs.readFileSync(home + "/.codex/session_index.jsonl", "utf8").includes("Codex renamed here") && (await names()).includes("Codex renamed here"), await names());

// Reopen.
ok("crashed conversations are counted", (await js(`document.getElementById('summary').textContent`)).includes("2 crashed"), await js(`document.getElementById('summary').textContent`));
await key("O", "KeyO"); await sleep(500);
ok("reopen lists both", (await js(`document.querySelectorAll('#dialog-body li').length`)) === 2);
await shot("24-reopen");
const before = log().length;
await js(`document.querySelector('#dialog-body input[type=checkbox]:not(:checked)')`);
await js(`document.querySelectorAll('#dialog-body input[type=checkbox]')[1].click()`);
ok("the count follows the ticks", (await js(`document.querySelector('#dialog-body p').textContent`)).startsWith("1 of 2"), await js(`document.querySelector('#dialog-body p').textContent`));
await js(`document.querySelector('#dialog-buttons .main').click()`); await sleep(1000);
ok("ticked ones start", log().length === before + 1, log());
ok("and it reports", (await js(`document.querySelector('#dialog-body').textContent`)).includes("1 unticked no longer show as crashed."), await js(`document.querySelector('#dialog-body').textContent`));
await shot("25-report");
await js(`document.querySelector('#dialog-buttons .main').click()`); await sleep(400);
ok("the unticked one stops showing as crashed", await js(`!document.getElementById('summary').textContent.includes('2 crashed')`), await js(`document.getElementById('summary').textContent`));

// A change from another website.
const victim = await selId();
const attacker = http.createServer((_, res) => { res.setHeader("Content-Type", "text/html"); res.end("<!doctype html><p>evil</p>"); }).listen(0, "127.0.0.1");
await sleep(100);
const count0 = log().length;
await go(`http://127.0.0.1:${attacker.address().port}/`);
for (const type of ["text/plain", "application/json"]) {
  await js(`fetch(${JSON.stringify(base + "/api/resume")},{method:'POST',mode:'no-cors',credentials:'include',headers:{'Content-Type':${JSON.stringify(type)}},body:JSON.stringify({id:${JSON.stringify(h)}})}).then(r=>r.type).catch(()=> 'blocked')`);
}
await js(`(()=>{const f=document.createElement('form');f.method='POST';f.action=${JSON.stringify(base + "/api/resume")};f.enctype='text/plain';const i=document.createElement('input');i.name='{"id":"${h}","x":"';i.value='"}';f.append(i);document.body.append(f);f.submit();})()`);
await sleep(900);
ok("a form or fetch from another website starts nothing", log().length === count0, log());
attacker.close();

await finish(["403", "blocked by CORS", "ERR_FAILED", "/api/resume", "favicon"]);
