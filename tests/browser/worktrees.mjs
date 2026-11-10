// The worktree view and the clean-up screen.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import { link, out, home, termLog, base, sleep, send, js, go, shot, key, ok, names, count, finish } from "./lib.mjs";

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
const wait = async (cond, ms = 12000) => { for (let i = 0; i < ms / 100; i++) { if (await cond()) return true; await sleep(100); } return false; };
const wtNames = () => js(`[...document.querySelectorAll('#wt-rows tr[data-id] .wt-folder')].map(t=>t.textContent)`);
const summary = () => js(`document.getElementById('summary').textContent`);
const wtRow = (t) => `[...document.querySelectorAll('#wt-rows tr[data-id]')].find(r=>r.textContent.includes(${JSON.stringify(t)}))`;
const clickWt = async (t) => { await js(`${wtRow(t)}.querySelector('.wt-branch').click()`); await sleep(150); };
const cleanCells = (k) => js(`[...document.querySelector('#cleanup-rows tr[data-key=${k}]').children].map(c=>c.textContent)`);
const exists = (p) => fs.existsSync(home + "/work/" + p);
const footText = () => js(`document.querySelector('.foot').textContent`);

// The list scrolls inside its pane, however many conversations there are.
ok("the page itself doesn't scroll with a long list", await js(`document.documentElement.scrollHeight <= innerHeight + 1 && document.getElementById('list').scrollHeight > document.getElementById('list').clientHeight`), await js(`[document.documentElement.scrollHeight, innerHeight, document.getElementById('list').scrollHeight, document.getElementById('list').clientHeight]`));

// Worktree view.
await key("w", "KeyW");
ok("w opens the view, and finds them", await wait(async () => (await wtNames()).length === 8), await wtNames());
ok("only the worktree screen shows", await js(`!document.getElementById('screen-worktrees').hidden && document.getElementById('screen-conversations').hidden && document.getElementById('tools').hidden`));
await wait(async () => !(await summary()).includes("checking"));
const sum = await summary();
ok("summary counts them like the terminal", sum.includes("8 in 1 repository") && sum.includes("1 with changes") && sum.includes("1 with unpushed commits"), sum);
ok("the repository is a group row", (await js(`document.querySelector('#wt-rows tr.group').textContent`)).startsWith("~/work/shop"), await js(`document.querySelector('#wt-rows tr.group').textContent`));
ok("the main worktree isn't a row", !(await wtNames()).includes("~/work/shop"), await wtNames());
ok("changes show", (await js(`${wtRow("shop-wip")}.querySelector('.wt-changes').textContent`)) === "1 file");
ok("unpushed show", (await js(`${wtRow("shop-ahead")}.querySelector('.wt-unpushed').textContent`)) === "1");
ok("conversations show", (await js(`${wtRow("shop-worked")}.querySelector('.wt-convs').textContent`)) === "1");
ok("locked shows", (await js(`${wtRow("shop-locked")}.querySelector('.wt-status').textContent`)) === "locked");
ok("gone shows as missing", (await js(`${wtRow("shop-gone")}.querySelector('.wt-status').textContent`)) === "missing");
await clickWt("shop-worked");
const d1 = await js(`document.getElementById('wt-details').textContent + document.getElementById('wt-worked').textContent`);
ok("details explain it and list who worked there", d1.includes("No uncommitted changes.") && d1.includes("Worked in a worktree") && d1.includes("100% of its activity") && d1.includes("started here"), d1);
await shot("40-worktrees");

// Cancelling changes nothing.
await clickWt("shop-tidy2");
await key("d", "KeyD"); await settle();
ok("d opens the removal popup", (await dialogOpen()) && (await js(`document.getElementById('dialog-title').textContent`)) === "Remove worktrees?");
ok("says branches stay", (await dbody()).includes("1 worktree will be removed. Branches stay, so commits are kept."), await dbody());
ok("Cancel has the focus", await js(`document.activeElement.textContent==='Cancel'`));
await key("Enter", "Enter"); await sleep(400);
ok("Enter on Cancel removes nothing", exists("shop-tidy2") && (await status()) === "Cancelled. Nothing was removed.", await status());
await key("d", "KeyD"); await settle();
await key("Escape", "Escape"); await sleep(400);
ok("Esc cancels too", exists("shop-tidy2") && !(await dialogOpen()));

// Ignored files are named.
await clickWt("shop-ignored");
await key("d", "KeyD"); await settle();
ok("the popup names ignored files and who worked there", (await dbody()).includes("Files Git ignores go too, 2K") === false ? (await dbody()).includes("secret.env") : true, await dbody());
ok("and the conversation", (await dbody()).includes("Ignored files chat") && (await dbody()).includes("secret.env"), await dbody());
await shot("41-remove-ignored");
await key("Escape", "Escape"); await sleep(300);

// Refusals.
await clickWt("shop-wip");
await key("d", "KeyD"); await settle();
ok("changes are refused with the reason", !(await dialogOpen()) && (await status()).startsWith("Can't remove:"), await status());
await clickWt("shop-locked");
await key("d", "KeyD"); await settle();
ok("locked is refused with the reason", !(await dialogOpen()) && (await status()).startsWith("Can't remove:") && exists("shop-locked"), await status());

// Del pause.
await clickWt("shop-tidy2");
await key("Delete", "Delete"); await settle();
await key("Delete", "Delete"); await sleep(250);
ok("a quick second Del waits", (await dialogOpen()) && (await footText()).startsWith("Too quick") && exists("shop-tidy2"), await footText());
await key("Escape", "Escape"); await sleep(300);

// Countdown, in the page and on the server.
await clickWt("shop-ahead");
await key("d", "KeyD"); await settle();
ok("unpushed commits warn", (await dbody()).includes("Warning: It has 1 unpushed commit"), await dbody());
ok("Remove is locked", await js(`document.querySelector('#dialog-buttons .danger').disabled`));
ok("says when it unlocks", (await footText()).startsWith("Remove unlocks in"), await footText());
await shot("42-countdown");
const planId = await js(`(async()=>{const r=await fetch('/api/worktrees/plan',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({paths:[${JSON.stringify(home + "/work/shop-ahead")}]})});return (await r.json()).plan_id})()`);
const forged = await js(`(async()=>{const r=await fetch('/api/delete/confirm',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({plan_id:${JSON.stringify(planId)}})});return await r.json()})()`);
ok("a request can't skip the wait", forged.ok === false && forged.restarted === true && exists("shop-ahead"), forged);
await key("Escape", "Escape"); await sleep(300);
await clickWt("shop-ahead");
await key("d", "KeyD"); await settle();
await key("y", "KeyY"); await sleep(300);
ok("y too early starts it over and removes nothing", (await footText()).startsWith("Too early, so it started over.") && exists("shop-ahead"), await footText());
ok("waits it out", await wait(async () => js(`!document.querySelector('#dialog-buttons .danger').disabled`), 8000));
await key("y", "KeyY");
ok("removes it after the wait", await wait(async () => !exists("shop-ahead")), gitwt());
ok("its commit stays on the branch", execFileSync("git", ["-C", home + "/work/shop", "log", "--oneline", "shop-ahead"]).toString().includes("two"));
ok("says the branch is kept", (await status()) === "Removed 1 worktree. Its branch is kept.", await status());
ok("the row goes", await wait(async () => !(await wtNames()).includes("~/work/shop-ahead")), await wtNames());

// A page can't name something else.
const outside = await js(`(async()=>{const r=await fetch('/api/worktrees/plan',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({paths:[${JSON.stringify(home + "/work/shop")}, ${JSON.stringify(home)}]})});return await r.json()})()`);
ok("the main worktree and other folders can't be named", outside.ok === false && outside.message === "Can't remove: nothing to remove.", outside);

// Marks.
await key("a", "KeyA"); await sleep(150);
ok("a marks every row", (await js(`document.querySelectorAll('#wt-rows tr.marked').length`)) === 7, await js(`document.querySelectorAll('#wt-rows tr.marked').length`));
ok("summary counts them", (await summary()).includes("7 marked"), await summary());
await key("Escape", "Escape"); await sleep(150);
ok("Esc clears marks first and stays", (await js(`document.querySelectorAll('#wt-rows tr.marked').length`)) === 0 && await js(`screen==='worktrees'`));
await clickWt("shop-tidy2");
await key(" ", "Space"); await sleep(150);
ok("Space marks and moves down", await js(`${wtRow("shop-tidy2")}.classList.contains('marked') && !${wtRow("shop-tidy2")}.classList.contains('selected')`));
await key(" ", "Space"); await sleep(150);
await key("Escape", "Escape"); await sleep(100);
await key("Escape", "Escape"); await sleep(300);
ok("Esc again goes back to the conversations", await js(`screen==='conversations' && !document.getElementById('screen-conversations').hidden`));
ok("the conversation list is back", (await count()) > 0);

// Clean-up screen.
await key("C", "KeyC");
ok("Shift+C opens clean-up", await js(`screen==='cleanup'`));
ok("job folders are counted", await wait(async () => (await cleanCells("jobs"))[2] === "2"), await cleanCells("jobs"));
ok("waits for worktrees", await wait(async () => (await cleanCells("worktrees"))[2] !== "…"), await cleanCells("worktrees"));
await wait(async () => !(await summary()).includes("checking"));
ok("conversations it never replied to", (await cleanCells("no_reply"))[2] === "2", await cleanCells("no_reply"));
ok("folders that are gone, counting the worktree just removed", (await cleanCells("folder_gone"))[2] === "2", await cleanCells("folder_gone"));
ok("large and old, with its size", (await cleanCells("large_old"))[2] === "1" && (await cleanCells("large_old"))[3] === "114M", await cleanCells("large_old"));
ok("worktrees with nothing to lose, measured", (await cleanCells("worktrees"))[2] === "3" && (await cleanCells("worktrees"))[3] !== "?", await cleanCells("worktrees"));
ok("the job with a worktree frees nothing", (await cleanCells("jobs"))[3].endsWith("K"), await cleanCells("jobs"));
ok("about text shows", (await js(`document.getElementById('cleanup-about').textContent`)).includes("Conversations where Claude never replied"), await js(`document.getElementById('cleanup-about').textContent`));
await shot("43-cleanup");

// Opening lines.
await key("ArrowDown", "ArrowDown"); await key("ArrowDown", "ArrowDown");
await key("Enter", "Enter"); await sleep(700);
ok("a line opens the list showing exactly those", (await names()).join() === "Huge old conversation", await names());
ok("and says which suggestion", (await summary()).includes("clean-up: over 100M not used for 30 days"), await summary());
await key("c", "KeyC"); await sleep(500);
ok("c clears it like any filter", (await count()) > 100 && !(await summary()).includes("clean-up"), await summary());
await key("C", "KeyC"); await sleep(800);
await key("End", "End"); await key("ArrowUp", "ArrowUp");
await wait(async () => (await cleanCells("worktrees"))[2] !== "…");
await key("Enter", "Enter");
ok("the worktree line opens only those", await wait(async () => (await wtNames()).length === 3), await wtNames());
ok("summary says so", (await summary()).includes("showing 3 with nothing to lose, of 7"), await summary());
await shot("44-only-idle");
await key("c", "KeyC");
ok("c shows them all again", await wait(async () => (await wtNames()).length === 7), await wtNames());
await key("C", "KeyC"); await sleep(800);

// Jobs.
await key("End", "End"); await sleep(150);
ok("the job line lists them", (await js(`document.getElementById('cleanup-about').textContent`)).includes("bbbbbbbb"), await js(`document.getElementById('cleanup-about').textContent`));
await key("Enter", "Enter"); await settle();
ok("Enter lists job folders in a popup", (await dbody()).includes("aaaaaaaa") && (await dbody()).includes("may have a worktree, kept") && (await dbody()).includes("1 job folder"), await dbody());
await shot("45-jobs");
await key("c", "KeyC"); await sleep(400);
ok("c copies the claude rm commands and deletes nothing", !(await dialogOpen()) && (await status()) === "Copied: claude rm bbbbbbbb" && fs.existsSync(home + "/.claude/jobs/aaaaaaaa"), await status());
await key("Enter", "Enter"); await settle();
await key("y", "KeyY");
ok("y deletes only the one without a worktree", await wait(async () => !fs.existsSync(home + "/.claude/jobs/aaaaaaaa")) && fs.existsSync(home + "/.claude/jobs/bbbbbbbb"));
ok("says how much", (await status()).startsWith("Deleted 1 job folder, freed"), await status());
ok("the line recounts", await wait(async () => (await cleanCells("jobs"))[2] === "1"), await cleanCells("jobs"));
await key("d", "KeyD"); await settle();
ok("d with only worktree jobs says how to remove them", !(await dialogOpen()) && (await status()).includes("copy the claude rm commands"), await status());
await key("Enter", "Enter"); await settle();
ok("Enter still lists it, with nothing to delete", (await dbody()).includes("can't delete any of these") && await js(`!document.querySelector('#dialog-buttons .danger')`), await dbody());
await key("y", "KeyY"); await sleep(250);
ok("y does nothing there", fs.existsSync(home + "/.claude/jobs/bbbbbbbb") && await dialogOpen());
await key("Escape", "Escape"); await sleep(300);

// Several lines in one confirmation.
await key("Home", "Home");
await key(" ", "Space"); await sleep(100);
ok("Space marks without moving", await js(`document.querySelector('#cleanup-rows tr[data-key=no_reply]').classList.contains('marked') && document.querySelector('#cleanup-rows tr[data-key=no_reply]').classList.contains('selected')`));
await js(`document.querySelector('#cleanup-rows tr[data-key=worktrees] .col-mark input').click()`); await sleep(100);
ok("marked lines are counted", (await summary()).includes("2 lines marked"), await summary());
const never = [...await transcript("Never answered A"), ...await transcript("Never answered B")];
ok("found what should go", never.length === 2, never);
await key("d", "KeyD"); await settle();
const pop = await dbody();
ok("one popup says it all", pop.includes("2 conversations and 3 worktrees") && pop.includes("From the clean-up suggestions:") && pop.includes("Worktrees to remove. Their branches stay") && pop.includes("~/work/shop-tidy2"), pop);
await shot("46-cleanup-delete");
await key("Escape", "Escape"); await sleep(300);
ok("cancelling leaves everything", fs.existsSync(never[0]) && exists("shop-tidy") && (await status()) === "Cancelled. Nothing was deleted.", await status());
await key("d", "KeyD"); await settle();
await key("y", "KeyY");
ok("everything goes", await wait(async () => !fs.existsSync(never[0]) && !fs.existsSync(never[1]) && !exists("shop-tidy") && !exists("shop-tidy2")), gitwt());
ok("Git's record of the missing one is cleared", !gitwt().includes("shop-gone"), gitwt());
ok("branches stay", execFileSync("git", ["-C", home + "/work/shop", "branch", "--list", "shop-tidy2"]).toString().includes("shop-tidy2"));
ok("one line says what happened", /Deleted 2 conversations, freed .*\. Removed 3 worktrees\. Their branches are kept\./.test(await status()), await status());
ok("marks are gone and it stays on the screen", (await js(`screen==='cleanup' && document.querySelectorAll('#cleanup-rows tr.marked').length===0`)));
ok("the lines recount", await wait(async () => (await cleanCells("no_reply"))[2] === "0" && (await cleanCells("worktrees"))[2] === "0"), await cleanCells("worktrees"));
ok("untouched worktrees stay", exists("shop-wip") && exists("shop-locked") && exists("shop-worked"), gitwt());

// Nothing left.
await key("Home", "Home"); await sleep(100);
await key("d", "KeyD"); await settle();
ok("an empty line has nothing to delete", (await status()) === "Nothing to delete on this line.", await status());

// Phone width.
await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 800, deviceScaleFactor: 1, mobile: false });
await sleep(300);
ok("clean-up fits a phone", await js(`document.documentElement.scrollWidth <= innerWidth`), await js(`[document.documentElement.scrollWidth, innerWidth]`));
await shot("47-phone-cleanup");
await key("w", "KeyW"); await sleep(1200);
ok("worktrees fit a phone", await js(`document.documentElement.scrollWidth <= innerWidth`), await js(`[document.documentElement.scrollWidth, innerWidth]`));
await shot("48-phone-worktrees");
await send("Emulation.clearDeviceMetricsOverride");
await sleep(200);

// From another website.
const attacker = http.createServer((_, res) => { res.setHeader("Content-Type", "text/html"); res.end("<!doctype html><p>evil</p>"); }).listen(0, "127.0.0.1");
await sleep(100);
await go(`http://127.0.0.1:${attacker.address().port}/`);
await js(`fetch(${JSON.stringify(base + "/api/worktrees/plan")},{method:'POST',mode:'no-cors',credentials:'include',headers:{'Content-Type':'text/plain'},body:JSON.stringify({paths:[${JSON.stringify(home + "/work/shop-worked")}]})}).catch(()=>0)`);
await js(`fetch(${JSON.stringify(base + "/api/cleanup/plan")},{method:'POST',mode:'no-cors',credentials:'include',headers:{'Content-Type':'text/plain'},body:JSON.stringify({lines:['large_old']})}).catch(()=>0)`);
await sleep(300); attacker.close();
ok("another website can't remove anything", exists("shop-worked"));

await finish(["403", "blocked by CORS", "ERR_FAILED", "ERR_BLOCKED", "favicon"]);
