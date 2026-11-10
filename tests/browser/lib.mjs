// What the suites share: a throwaway headless Chrome, driven over its debugging port, and small
// helpers for the page. run.sh passes the link and the invented folders.
import { spawn } from "node:child_process";
import fs from "node:fs";
export const [link, out, home, project, termLog] = process.argv.slice(2);
export const base = new URL(link).origin;
export const port = 9400 + Math.floor(Math.random() * 400);
export const profile = fs.mkdtempSync(out + "/profile-");
export const chrome = spawn(process.env.CHROME || "google-chrome", ["--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "--no-first-run", "--window-size=1500,900", ...(process.env.CI ? ["--no-sandbox"] : []), "about:blank"], { stdio: "ignore" });
process.on("exit", () => chrome.kill());
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let target;
for (let i = 0; i < 80 && !target; i++) { await sleep(150); try { target = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find((t) => t.type === "page"); } catch {} }
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((r) => ws.onopen = r);
let seq = 0; const waiting = new Map();
export const problems = [];
ws.onmessage = (m) => { const d = JSON.parse(m.data);
  if (d.id && waiting.has(d.id)) { waiting.get(d.id)(d); waiting.delete(d.id); }
  if (d.method === "Runtime.exceptionThrown") problems.push("EXC " + JSON.stringify(d.params.exceptionDetails).slice(0, 300));
  if (d.method === "Runtime.consoleAPICalled" && d.params.type === "error") problems.push("CON " + JSON.stringify(d.params.args).slice(0, 300));
  if (d.method === "Log.entryAdded" && d.params.entry.level === "error") problems.push("LOG " + d.params.entry.text + " " + (d.params.entry.url || ""));
};
export const send = (method, params = {}) => new Promise((r) => { const id = ++seq; waiting.set(id, r); ws.send(JSON.stringify({ id, method, params })); });
for (const d of ["Page", "Runtime", "Log", "Network"]) await send(d + ".enable");
export const js = async (e) => { const r = await send("Runtime.evaluate", { expression: e, awaitPromise: true, returnByValue: true }); if (r.result.exceptionDetails) throw new Error(e + " :: " + JSON.stringify(r.result.exceptionDetails)); return r.result.result.value; };
export const go = async (u) => { await send("Page.navigate", { url: u }); await sleep(900); };
export const shot = async (n) => { const r = await send("Page.captureScreenshot", { format: "png" }); fs.writeFileSync(`${out}/${n}.png`, Buffer.from(r.result.data, "base64")); };
export const key = async (k, code = k, text = k === "Enter" ? "\r" : k.length === 1 ? k : undefined) => { await send("Input.dispatchKeyEvent", { type: "keyDown", key: k, code, text, windowsVirtualKeyCode: ({Escape:27,Enter:13,Delete:46," ":32})[k] }); await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code }); await sleep(250); };
let bad = 0;
export const ok = (name, cond, got) => { console.log((cond ? "ok   " : "FAIL ") + name + (cond ? "" : "  got: " + JSON.stringify(got))); if (!cond) bad++; };
export const names = () => js(`[...document.querySelectorAll('#rows tr[data-id] .col-name')].map(t=>t.firstChild.textContent)`);
export const count = () => js(`document.querySelectorAll('#rows tr[data-id]').length`);

// Ends the run. `allowed` lists what may show as a browser error, such as requests a check blocks on purpose.
export const finish = async (allowed) => {
  const real = problems.filter((p) => !allowed.some((a) => p.includes(a)));
  ok("no script errors or blocked resources", real.length === 0, real);
  console.log(bad ? `${bad} FAILED` : "ALL PASSED");
  chrome.kill(); await sleep(300); fs.rmSync(profile, { recursive: true, force: true });
  process.exit(bad ? 1 : 0);
};
