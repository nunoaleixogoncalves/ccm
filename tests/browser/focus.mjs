// The "Focus new conversation" setting. Here ccm thinks it's on Hyprland, and a fake hyprctl logs what it's asked to do.
import fs from "node:fs";
import { link, out, home, termLog, base, sleep, send, js, go, key, ok, finish } from "./lib.mjs";

await go(link); await sleep(1500);
const pick = async (text) => { await js(`[...document.querySelectorAll('#rows tr[data-id]')].find(t=>t.textContent.includes(${JSON.stringify(text)})).click()`); await sleep(450); };
const read = (file) => { try { return fs.readFileSync(file, "utf8"); } catch { return ""; } };
const opened = () => read(termLog).trim().split("\n").filter(Boolean).length;
const steered = () => read(`${out}/hyprctl.log`).split("\n").filter((l) => l.startsWith("dispatch focuswindow")).length;
const config = () => read(`${home}/.config/ccm/config.toml`);
const checked = () => js(`document.getElementById('focus-new').checked`);
const status = () => js(`document.getElementById('status-text').textContent`);
const until = async (test) => { for (let i = 0; i < 60 && !test(); i++) await sleep(100); return test(); };

ok("on by default", await checked());
ok("the box doesn't say it can't work", await js(`!document.getElementById('focus-chip').classList.contains('cannot')`));
ok("nothing is saved until it's changed", !config().includes("focus_new_conversation"), config());

await pick("Fix the checkout total");
await key("Enter", "Enter");
ok("resume opens it", await until(() => opened() === 1), opened());
await sleep(600);
ok("with it on, the new window keeps the focus", read(`${out}/hyprctl.log`) === "", read(`${out}/hyprctl.log`));

await js(`document.getElementById('focus-new').click()`); await sleep(500);
ok("the box unticks", !(await checked()));
ok("it's saved in config.toml", config().includes("focus_new_conversation = false"), config());
ok("the rest of the file stays", config().includes("terminal = ["), config());
ok("it says what changed", (await status()).includes("focus stays on ccm"), await status());
ok("the box gives the keyboard back", await js(`document.activeElement !== document.getElementById('focus-new')`));

await pick("Huge old conversation");
await key("Enter", "Enter");
ok("resume opens it with the setting off", await until(() => opened() === 2), opened());
ok("with it off, the focus comes back", await until(() => steered() === 1), read(`${out}/hyprctl.log`));
ok("to the window that had it", read(`${out}/hyprctl.log`).includes("dispatch focuswindow address:0xa"), read(`${out}/hyprctl.log`));

await key("F", "KeyF", "F"); await sleep(500);
ok("the key turns it back on", await checked());
ok("and saves it once", (config().match(/focus_new_conversation/g) || []).length === 1 && config().includes("focus_new_conversation = true"), config());

await key("O", "KeyO"); await sleep(500);
const before = steered();
await js(`document.querySelector('#dialog-buttons .main').click()`);
ok("reopening stays on ccm even with it on", await until(() => steered() > before), read(`${out}/hyprctl.log`));
await js(`document.querySelector('#dialog-buttons .main').click()`); await sleep(400);

fs.writeFileSync(`${home}/.config/ccm/config.toml`, config().replace("focus_new_conversation = true", "focus_new_conversation = false"));
await sleep(3500);
ok("a change from the terminal app shows on the page", !(await checked()));

await go(base + "/"); await sleep(1200);
ok("it's still off after a reload", !(await checked()));

await finish(["favicon"]);
