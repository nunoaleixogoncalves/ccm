"use strict";

// Conversations are untrusted text. Everything here goes through textContent, never as markup.

const POLL_MS = 2000;
const AGES_MS = 30000;
const SEARCH_WAIT_MS = 120;
const DETAILS_WAIT_MS = 60;
const PAGE_ROWS = 10;
const FORK_WAIT_MS = 15 * 60 * 1000;
const MAX_NAME = 200;
const MAX_TAGS_TEXT = 800;
// Two clicks this close together are a double-click, so a second one isn't its own decision.
const DOUBLE_CLICK_MS = 400;
// Del both opens and confirms a delete, so a held key must not do both.
const DEL_PAUSE_MS = 1000;
const GROUPS = ["off", "folder", "repo"];
const GROUP_LABELS = { off: "Group", folder: "Group: folder", repo: "Group: repo" };

const $ = (id) => document.getElementById(id);

const view = {
  filters: new Set(),
  tool: null,
  sort: "active",
  desc: true,
  group: "off",
  q: "",
  fresh: false,
};

let columns = [];
let tools = [];
let rows = [];
let drawnRows = "";
let drawnColumns = "";
let selected = null;
let selectedIndex = 0;
let version = null;
let focusState = { on: false, note: null };
// Counts changes to the setting made here, so an answer asked for before one can't undo it.
let focusChanges = 0;
let listRequest = 0;
let detailsRequest = 0;
let detailsTimer = 0;
let searchTimer = 0;
let shownDetails = null;
// What the detail pane shows, for the actions on it.
let current = null;
// A fork just started, to select once its tool saves it.
let awaitingFork = null;
// Conversations picked for a delete, which otherwise acts on the selected one.
const marked = new Set();
let summaryText = "";
// What the open dialog does with a key, and when it's dismissed.
let dialogKeys = null;
let dialogCancel = null;
let closingDialog = false;
// A delete is being worked out, which reads every conversation. Keys wait, so one pressed meanwhile
// can't act on something else.
let planning = false;
// Which screen is showing. Each keeps its own selection and marks.
let screen = "conversations";
let screenRequest = 0;
let wtRows = [];
let wtSelected = null;
let wtIndex = 0;
let wtSummary = "";
let drawnWt = "";
let drawnWtDetails = "";
let onlyIdle = false;
const wtMarked = new Set();
let cleanupLines = [];
let cleanupRow = 0;
let cleanupSummary = "";
let drawnCleanup = "";
let drawnAbout = "";
const cleanupMarked = new Set();

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  Object.assign(node, props);
  node.append(...children);
  return node;
}

function banner(text) {
  $("banner").textContent = text || "";
  $("banner").hidden = !text;
}

async function api(path) {
  let response;
  try {
    response = await fetch(path, { cache: "no-store" });
  } catch {
    banner("ccm web has stopped. Start it again to carry on.");
    return null;
  }
  if (response.status === 401 || response.status === 403) {
    banner(await response.text());
    return null;
  }
  if (response.status === 404) {
    return null;
  }
  banner("");
  return response.json();
}

async function post(path, body) {
  let response;
  try {
    response = await fetch(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  } catch {
    banner("ccm web has stopped. Start it again to carry on.");
    return null;
  }
  if (response.status === 401 || response.status === 403) {
    banner(await response.text());
    return null;
  }
  if (!response.ok) {
    say(await response.text(), true);
    return null;
  }
  return response.json();
}

// The latest outcome. It stays until the next one, as the terminal app's status line does.
function say(text, bad = false, command = null) {
  $("status").hidden = !text;
  $("status").classList.toggle("bad", bad);
  $("status-text").textContent = text || "";
  $("status-copy").hidden = !command;
  $("status-copy").dataset.command = command || "";
}

function address() {
  const params = new URLSearchParams();
  if (view.filters.size) params.set("filters", [...view.filters].join(","));
  if (view.tool) params.set("tool", view.tool);
  params.set("sort", view.sort);
  params.set("desc", view.desc ? "1" : "0");
  if (view.group !== "off") params.set("group", view.group);
  if (view.q) params.set("q", view.q);
  if (view.fresh) params.set("fresh", "1");
  view.fresh = false;
  return "/api/list?" + params;
}

async function loadList() {
  const request = ++listRequest;
  const changes = focusChanges;
  const data = await api(address());
  if (!data || request !== listRequest) return;
  version = data.version;
  columns = data.columns;
  tools = data.tools;
  rows = data.rows;
  drawSummary(data);
  if (changes === focusChanges) drawFocus(data.focus);
  drawHead();
  drawRows();
  drawChips();
  drawEmpty(data);
  keepSelection();
  drawMarks();
  loadDetails();
}

function drawFocus(focus) {
  focusState = focus;
  $("focus-new").checked = focus.on && !focus.note;
  $("focus-chip").classList.toggle("cannot", !!focus.note);
  $("focus-chip").title = focus.note || "On: Resume and Fork go to the new conversation. Off: focus stays here.";
}

async function toggleFocus() {
  if (focusState.note) {
    say(focusState.note);
    drawFocus(focusState);
    return;
  }
  // Before and after, so an answer asked for while saving is dropped too.
  focusChanges++;
  const reply = await post("/api/focus", { on: !focusState.on });
  focusChanges++;
  if (!reply) {
    drawFocus(focusState);
    return;
  }
  drawFocus(reply.ok ? { on: reply.on, note: null } : focusState);
  say(reply.message, !reply.ok);
}

function drawSummary(data) {
  const parts = [`${data.total} conversations`, `${data.total_size} on disk`, `${data.open} open`];
  if (data.filtered) parts.push(`showing ${data.shown}`);
  if (data.crashed) parts.push(`${data.crashed} crashed`);
  if (data.suggestion) parts.push(`clean-up: ${data.suggestion}`);
  if (data.loading) parts.push("scanning…");
  if (data.notice) parts.push(data.notice);
  summaryText = parts.join("  ·  ");
}

function bytes(n) {
  const K = 1024, M = K * K, G = M * K;
  if (n < K) return `${n}B`;
  if (n < M) return `${Math.floor(n / K)}K`;
  if (n < G) return `${Math.floor(n / M)}M`;
  return `${(n / G).toFixed(1)}G`;
}

function markedShown() {
  return items().filter((row) => marked.has(row.id));
}

// Marks change far more often than the rows, and don't need them drawn again.
function drawMarks() {
  for (const tr of $("rows").querySelectorAll("tr[data-id]")) {
    const on = marked.has(tr.dataset.id);
    tr.classList.toggle("marked", on);
    tr.querySelector(".col-mark input").checked = on;
  }
  const shown = markedShown();
  const size = shown.reduce((sum, row) => sum + row.bytes, 0);
  $("summary").textContent = summaryText + (shown.length ? `  ·  ${shown.length} marked ${bytes(size)}` : "");
  const button = $("delete-button");
  if (button) {
    button.firstChild.textContent = shown.length ? `Delete ${shown.length} ` : "Delete ";
    button.disabled = !shown.length && !(current && current.can.delete);
  }
}

function toggleMark(id) {
  if (!marked.delete(id)) marked.add(id);
  drawMarks();
}

function markAndMove() {
  if (selected === null) return;
  toggleMark(selected);
  move(1);
}

// Marks everything shown, or clears those marks if they're all set already.
function markAll() {
  const shown = items().map((row) => row.id);
  if (shown.every((id) => marked.has(id))) shown.forEach((id) => marked.delete(id));
  else shown.forEach((id) => marked.add(id));
  drawMarks();
}

function drawHead() {
  const state = JSON.stringify([columns, view.sort, view.desc]);
  if (state === drawnColumns) return;
  drawnColumns = state;
  const markHead = el("th", { className: "col-mark", scope: "col", ariaLabel: "Marked" });
  $("head").replaceChildren(markHead, ...columns.map((column) => {
    const sorted = column.key === view.sort;
    const button = el("button", { textContent: column.title + (sorted ? (view.desc ? " ▼" : " ▲") : "") });
    button.addEventListener("click", () => sortBy(column));
    const cell = el("th", { className: "col-" + column.key, scope: "col" }, button);
    if (sorted) cell.setAttribute("aria-sort", view.desc ? "descending" : "ascending");
    return cell;
  }));
}

function rowClass(cells) {
  switch (cells.status) {
    case "open": return "is-open";
    case "crashed": return "is-crashed";
    case "missing":
    case "junk": return "is-dim";
    default: return "";
  }
}

function drawRow(row) {
  if (row.header !== undefined) {
    const count = el("span", { className: "count", textContent: `${row.count}  ·  ${row.size}` });
    return el("tr", { className: "group" }, el("td", { colSpan: columns.length + 1, textContent: row.header }, count));
  }
  const tr = el("tr", { className: rowClass(row.cells) });
  tr.dataset.id = row.id;
  tr.append(el("td", { className: "col-mark" }, el("input", { type: "checkbox", tabIndex: -1, ariaLabel: `Mark ${row.cells.name}` })));
  for (const column of columns) {
    const td = el("td", { className: "col-" + column.key, title: row.cells[column.key] });
    if (column.key === "folder") {
      td.append(el("bdi", { textContent: row.cells.folder }));
    } else {
      td.textContent = row.cells[column.key];
    }
    if (column.key === "name" && row.pinned) {
      td.prepend(el("span", { className: "pin", textContent: "◆", title: "Pinned" }));
    }
    if (column.key === "name" && row.tag) {
      td.append(el("span", { className: "tag", textContent: row.tag }));
    }
    if (column.key === "name") {
      // Clicking one searches for it.
      for (const tag of row.tags) {
        td.append(el("button", { type: "button", className: "label", tabIndex: -1, textContent: "#" + tag, title: `Show only #${tag}` }));
      }
    }
    tr.append(td);
  }
  return tr;
}

function drawRows() {
  const state = JSON.stringify([rows, columns]);
  if (state === drawnRows) return;
  drawnRows = state;
  const list = $("list");
  const top = list.scrollTop;
  $("rows").replaceChildren(...rows.map(drawRow));
  list.scrollTop = top;
}

function drawEmpty(data) {
  let message = "";
  if (!rows.length) {
    if (data.total) message = "No conversations match. Clear the search or the filters.";
    else if (data.loading) message = "Scanning…";
    else message = `No conversations found in ${data.looked_in}`;
  }
  $("empty").textContent = message;
  $("empty").hidden = !message;
}

function drawChips() {
  for (const chip of document.querySelectorAll("[data-filter]")) {
    chip.setAttribute("aria-pressed", String(view.filters.has(chip.dataset.filter)));
  }
  const tool = $("tool");
  tool.hidden = tools.length < 2;
  const picked = tools.find((t) => t.key === view.tool);
  tool.firstElementChild.textContent = picked ? `Tool: ${picked.label}` : "Tool";
  tool.setAttribute("aria-pressed", String(Boolean(picked)));
  const group = $("group");
  group.firstElementChild.textContent = GROUP_LABELS[view.group];
  group.setAttribute("aria-pressed", String(view.group !== "off"));
}

function items() {
  return rows.filter((row) => row.id !== undefined);
}

function keepSelection() {
  const all = items();
  if (awaitingFork) {
    if (all.some((row) => row.id === awaitingFork.id)) {
      selected = awaitingFork.id;
      awaitingFork = null;
    } else if (Date.now() > awaitingFork.until) {
      awaitingFork = null;
    }
  }
  if (!all.some((row) => row.id === selected)) {
    const previous = selectedIndex;
    selected = all.length ? all[Math.min(previous, all.length - 1)].id : null;
  }
  markSelected();
}

function markSelected() {
  selectedIndex = Math.max(0, items().findIndex((row) => row.id === selected));
  for (const tr of $("rows").querySelectorAll("tr.selected")) {
    tr.classList.remove("selected");
    tr.removeAttribute("aria-selected");
  }
  const tr = selectedRow();
  if (tr) {
    tr.classList.add("selected");
    tr.setAttribute("aria-selected", "true");
  }
}

function selectedRow() {
  return selected === null ? null : $("rows").querySelector(`tr[data-id="${CSS.escape(selected)}"]`);
}

function select(id) {
  if (id === selected) return;
  selected = id;
  markSelected();
  selectedRow()?.scrollIntoView({ block: "nearest" });
  clearTimeout(detailsTimer);
  detailsTimer = setTimeout(loadDetails, DETAILS_WAIT_MS);
}

function move(delta) {
  const all = items();
  if (!all.length) return;
  const at = all.findIndex((row) => row.id === selected);
  const next = Math.min(all.length - 1, Math.max(0, (at < 0 ? 0 : at) + delta));
  select(all[next].id);
}

async function loadDetails() {
  const request = ++detailsRequest;
  if (selected === null) {
    drawDetails(null);
    return;
  }
  const data = await api("/api/details?id=" + encodeURIComponent(selected));
  if (request === detailsRequest) drawDetails(data);
}

function fact(list, label, value, className = "") {
  list.append(el("dt", { textContent: label }), el("dd", { textContent: value, className }));
}

function actionButton(label, key, action, enabled = true, main = false) {
  const button = el("button", { type: "button", className: main ? "chip main" : "chip", disabled: !enabled }, label + " ", el("kbd", { textContent: key }));
  button.addEventListener("click", action);
  return button;
}

function deleteButton(data) {
  const button = actionButton("Delete ", "d", startDelete, data.can.delete);
  button.id = "delete-button";
  return button;
}

function drawDetails(data) {
  current = data;
  const details = $("details");
  const preview = $("preview");
  if (!data) {
    shownDetails = null;
    details.replaceChildren();
    preview.replaceChildren();
    return;
  }
  const state = JSON.stringify(data);
  if (state === shownDetails) return;
  const sameConversation = details.dataset.id === data.id;
  const fromBottom = preview.scrollHeight - preview.scrollTop;
  shownDetails = state;
  details.dataset.id = data.id;

  const facts = el("dl");
  fact(facts, "ID", data.id, "mono");
  fact(facts, "Tool", data.tool);
  fact(facts, "Folder", data.folder + (data.missing ? "  (missing)" : ""), data.missing ? "mono warn" : "mono");
  fact(facts, "Started", data.started);
  fact(facts, "Cost", data.cost);
  fact(facts, "Lines", data.lines);
  fact(facts, "PR", data.pr);
  if (data.tags.length) fact(facts, "Tags", data.tags.map((tag) => "#" + tag).join(" "));
  const actions = el("div", { className: "actions" },
    actionButton("Resume", "Enter", () => resume(), data.can.resume, true),
    actionButton("Fork", "f", () => resume(true), data.can.fork),
    actionButton("Rename", "R", startRename, data.can.rename),
    actionButton(data.pinned ? "Unpin" : "Pin", "p", togglePin),
    actionButton("Tags", "T", startTags),
    deleteButton(data),
    actionButton("Copy ID", "y", () => copy(data.id)),
    actionButton("Copy command", "Y", () => copy(data.command)));
  details.replaceChildren(el("h2", { textContent: data.name }), actions, facts);
  drawMarks();
  if (data.notes.length) {
    details.append(el("ul", { className: "notes" }, ...data.notes.map((note) => el("li", { textContent: note }))));
  }

  const parts = data.recaps.map((recap) => el("p", { className: "recap", textContent: recap }));
  if (data.error) {
    parts.push(el("p", { textContent: data.error }));
  } else if (!data.turns.length) {
    parts.push(el("p", { className: "older", textContent: "No messages." }));
  } else if (data.truncated) {
    parts.push(el("p", { className: "older", textContent: "… older messages not shown" }));
  }
  for (const turn of data.turns) {
    const heading = el("h3", { textContent: turn.who });
    if (turn.tool_calls) {
      heading.append(el("span", { textContent: `${turn.tool_calls} tool call${turn.tool_calls === 1 ? "" : "s"}` }));
    }
    parts.push(el("article", { className: turn.you ? "turn you" : "turn" }, heading, el("p", { textContent: turn.text })));
  }
  preview.replaceChildren(...parts);
  // Starts at the newest message, and a refresh leaves the reader where they were.
  preview.scrollTop = preview.scrollHeight - (sameConversation ? fromBottom : 0);
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    say(`Copied: ${text}`);
  } catch {
    say(`The browser wouldn't copy it, so here it is: ${text}`, true);
  }
}

function openDialog(title, body, buttons, onSubmit, keys = null, onCancel = () => say("Cancelled.")) {
  $("dialog-title").textContent = title;
  $("dialog-body").replaceChildren(...body);
  $("dialog-buttons").replaceChildren(...buttons);
  dialogSubmit = onSubmit;
  dialogKeys = keys;
  dialogCancel = onCancel;
  if (!$("dialog").open) $("dialog").showModal();
}

function closeDialog() {
  closingDialog = true;
  $("dialog").close();
}

function dialogButton(label, action, primary = false) {
  const button = el("button", { type: primary ? "submit" : "button", className: primary ? "chip main" : "chip", textContent: label });
  if (!primary) button.addEventListener("click", action);
  return button;
}

function cancelButton() {
  return dialogButton("Cancel", () => { closeDialog(); dialogCancel(); });
}

let dialogSubmit = null;

// Every check is made again on the server. Being told a folder is gone is the one question back.
async function resume(fork = false, recreate = false, id = selected) {
  if (id === null) return;
  const result = await post("/api/resume", { id, fork, recreate });
  if (!result) return;
  if (result.ask_folder) {
    askFolder(id, fork, result.ask_folder);
    return;
  }
  say(result.message, !result.ok, result.command);
  if (result.fork_id) awaitingFork = { id: result.fork_id, until: Date.now() + FORK_WAIT_MS };
  loadList();
}

function askFolder(id, fork, folder) {
  const doing = fork ? "fork" : "resume";
  const cancel = cancelButton();
  openDialog(
    "This folder no longer exists",
    [el("p", { className: "mono", textContent: folder }), el("p", { textContent: `Recreate it and ${doing}?` })],
    [cancel, dialogButton(`Recreate and ${doing}`, null, true)],
    () => { closeDialog(); resume(fork, true, id); },
  );
  cancel.focus();
}

function startRename() {
  if (!current) return;
  if (!current.can.rename) {
    say("This conversation is open, so rename it from there with /rename.", true);
    return;
  }
  const id = current.id;
  const input = el("input", { type: "text", maxLength: MAX_NAME, value: current.name === "(untitled)" ? "" : current.name, spellcheck: false, ariaLabel: "New name" });
  openDialog("Rename this conversation", [input], [cancelButton(), dialogButton("Save", null, true)], async () => {
    closeDialog();
    const result = await post("/api/rename", { id, name: input.value });
    if (!result) return;
    say(result.message, !result.ok);
    loadList();
  });
  input.select();
}

// The marked rows shown, or else the selected one, as pin and tags act on.
function targets() {
  const shown = markedShown();
  if (shown.length) return shown;
  return items().filter((row) => row.id === selected);
}

async function togglePin() {
  const ids = targets().map((row) => row.id);
  if (!ids.length) return;
  const result = await post("/api/pin", { ids });
  if (!result) return;
  say(result.message, !result.ok);
  loadList();
  loadDetails();
}

function searchTag(tag) {
  const words = view.q.split(/\s+/).filter(Boolean);
  if (!words.includes("#" + tag)) words.push("#" + tag);
  $("search").value = words.join(" ");
  setQuery($("search").value);
}

// With several, the box starts with the tags they all share, and only those change.
function startTags() {
  const picked = targets();
  if (!picked.length) return;
  const ids = picked.map((row) => row.id);
  const shared = picked[0].tags.filter((tag) => picked.every((row) => row.tags.includes(tag)));
  const before = shared.map((tag) => "#" + tag).join(" ");
  const input = el("input", { type: "text", maxLength: MAX_TAGS_TEXT, value: before ? before + " " : "", spellcheck: false, ariaLabel: "Tags" });
  const hint = el("p", { className: "hint", textContent: ids.length === 1
    ? "Spaces between tags. Leave it empty for none."
    : "These are the tags they all have. Tags only some have stay." });
  const problem = el("p", { className: "problem", hidden: true });
  const title = ids.length === 1 ? "Tags for this conversation" : `Tags for ${ids.length} conversations`;
  openDialog(title, [input, hint, problem], [cancelButton(), dialogButton("Save", null, true)], async () => {
    const result = await post("/api/tags", { ids, before, tags: input.value });
    if (!result) return;
    if (!result.ok) {
      problem.textContent = result.message;
      problem.hidden = false;
      input.focus();
      return;
    }
    closeDialog();
    say(result.message);
    loadList();
    loadDetails();
  });
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
}

async function startReopen() {
  const data = await api("/api/reopen");
  if (!data) return;
  if (!data.rows.length) {
    say(data.message);
    return;
  }
  const lead = el("p");
  const boxes = data.rows.map((row) => ({ id: row.id, box: el("input", { type: "checkbox", checked: !row.missing }) }));
  const count = () => { lead.textContent = `${boxes.filter((b) => b.box.checked).length} of ${boxes.length} will be resumed. Unticked ones stop showing as crashed.`; };
  boxes.forEach((b) => b.box.addEventListener("change", count));
  count();
  const list = el("ul", {}, ...data.rows.map((row, i) => {
    const parts = [boxes[i].box, el("span", { textContent: row.name }), el("span", { className: "where", textContent: row.folder })];
    if (row.missing) parts.push(el("span", { className: "warn", textContent: "folder missing, recreated if ticked" }));
    return el("li", {}, el("label", {}, ...parts));
  }));
  openDialog("Reopen crashed conversations", [lead, list], [cancelButton(), dialogButton("Reopen", null, true)], async () => {
    closeDialog();
    const ticked = boxes.filter((b) => b.box.checked).map((b) => b.id);
    const unticked = boxes.filter((b) => !b.box.checked).map((b) => b.id);
    const result = await post("/api/reopen", { ticked, unticked });
    if (!result) return;
    openDialog("Reopen finished", result.lines.map((line) => el("p", { textContent: line })), [dialogButton("OK", null, true)], closeDialog);
    loadList();
  });
}

function joined(parts) {
  return parts.length > 1 ? `${parts.slice(0, -1).join(", ")} and ${parts.at(-1)}` : parts.join("");
}

// Slow with huge conversations or big worktrees, so keys wait until it's done.
async function askPlan(path, body) {
  if (planning) return;
  planning = true;
  say("Working out what would go…");
  const plan = await post(path, body);
  planning = false;
  say("");
  if (!plan) return;
  if (!plan.ok) {
    say(plan.message, true);
    return;
  }
  showDeletePlan(plan);
}

function startDelete() {
  const shown = markedShown().map((row) => row.id);
  if (!shown.length && !(current && current.can.delete)) {
    say("This conversation is open, so it can't be deleted.", true);
    return;
  }
  return askPlan("/api/delete/plan", { ids: shown.length ? shown : [selected] });
}

function section(title) {
  return el("h3", { textContent: title });
}

function toneLine(line) {
  return el("p", { className: `line tone-${line.tone}`, textContent: line.text });
}

function jobList(jobs) {
  return el("ul", { className: "jobs" }, ...jobs.map((job) => el("li", {},
    el("span", { className: "job-id", textContent: job.short }),
    el("span", { className: "job-size", textContent: job.size }),
    el("span", { className: "job-age", textContent: job.age }),
    el("span", { textContent: job.what }),
    el("span", { className: "job-note", textContent: job.note }))));
}

function showDeletePlan(plan) {
  const removal = plan.kind === "remove";
  const verb = removal ? "Remove" : "Delete";
  const opened = Date.now();
  let delPressed = opened;
  let notes = false;
  let worktrees = false;
  let unlockAt = null;
  let restarted = false;
  let busy = false;
  let timer = 0;

  const lead = el("p", { className: "tone-bold" });
  const foot = el("p", { className: "foot", role: "status" });
  const body = removal ? plan.head.map(toneLine) : [lead];
  if (plan.from.length) {
    body.push(el("p", { className: "line", textContent: "From the clean-up suggestions:" }), ...plan.from.map((line) => el("p", { className: "line", textContent: `  ${line}` })));
  }
  if (plan.conversations.length) {
    body.push(el("ul", {}, ...plan.conversations.map((c) => el("li", { className: "row" },
      el("span", { textContent: c.name }), el("span", { className: "row-id", textContent: c.short }), el("span", { className: "row-size", textContent: bytes(c.bytes) })))));
  }
  for (const line of [...plan.skipped, ...plan.warnings]) body.push(el("p", { className: "line", textContent: line }));

  const notesHead = section("");
  const notesBox = el("input", { type: "checkbox" });
  if (plan.notes.length) {
    body.push(notesHead, el("ul", {}, ...plan.notes.map((name) => el("li", { className: "line", textContent: name }))),
      el("label", { className: "toggle" }, notesBox, el("span", { textContent: "Delete these notes too " }), el("kbd", { textContent: "m" })));
  }

  const treesHead = section("");
  const treesBox = el("input", { type: "checkbox" });
  if (plan.worktree_count || plan.stays.length) {
    body.push(treesHead, ...plan.worktrees.map((line) => el("p", { className: `line tone-${line.tone}`, textContent: line.text })));
    for (const line of plan.stays) body.push(el("p", { className: "line", textContent: line }));
    if (plan.worktree_count) {
      body.push(el("label", { className: "toggle" }, treesBox, el("span", { textContent: "Remove these worktrees too " }), el("kbd", { textContent: "w" })));
    }
  }
  body.push(...plan.removing.map(toneLine));
  if (plan.jobs.length) {
    body.push(section(plan.kept_jobs ? "Job folders. Ones that may have a worktree are kept, so remove them with claude rm:" : "Job folders to delete:"), jobList(plan.jobs));
  }
  body.push(foot);

  const deleteBtn = el("button", { type: "submit", className: "chip danger" }, `${verb} `, el("kbd", { textContent: "y" }));
  const cancel = cancelButton();

  function secondsLeft() {
    return unlockAt === null ? 0 : Math.max(0, Math.ceil((unlockAt - Date.now()) / 1000));
  }

  function update() {
    const count = plan.conversations.length;
    const trees = (n) => `${n} worktree${n === 1 ? "" : "s"}`;
    const what = count || !(plan.removing_count || plan.job_count) ? [`${count} conversation${count === 1 ? "" : "s"}`] : [];
    if (worktrees) what.push(trees(plan.worktree_count));
    if (plan.removing_count) what.push(trees(plan.removing_count));
    if (plan.job_count) what.push(`${plan.job_count} job folder${plan.job_count === 1 ? "" : "s"}`);
    const freed = plan.bytes.conversations + (notes ? plan.bytes.notes : 0) + (worktrees ? plan.bytes.worktrees : 0) + plan.bytes.removing + plan.bytes.jobs;
    lead.textContent = plan.nothing ? "ccm can't delete any of these. They may have worktrees, so remove them with claude rm." : `${joined(what)}, ${bytes(freed)} freed. This can't be undone.`;
    notesHead.textContent = `Memory notes nothing else wrote ${notes ? "will be deleted" : "will be kept"}:`;
    treesHead.textContent = !plan.worktree_count ? "Worktrees nothing else used:"
      : worktrees ? "Worktrees nothing else used will be removed too. Their branches stay, so commits are kept:"
      : "Worktrees nothing else used are kept unless you tick the box:";
    const left = secondsLeft();
    deleteBtn.disabled = busy || left > 0;
    if (left > 0) {
      const again = restarted ? "Too early, so it started over. " : "";
      foot.textContent = removal
        ? `${again}Remove unlocks in ${left}s. Pressing y or Del before then starts it over.`
        : `${again}Delete unlocks in ${left}s, because of the unpushed commits. Pressing y or Del before then starts it over. Unticking the worktrees ends the wait.`;
    } else if (!busy && foot.dataset.note !== "keep") {
      foot.textContent = "";
    }
  }

  function setCountdown(seconds) {
    unlockAt = seconds ? Date.now() + seconds * 1000 : null;
    clearInterval(timer);
    if (seconds) {
      timer = setInterval(() => {
        update();
        if (secondsLeft() === 0) clearInterval(timer);
      }, 200);
    }
    update();
  }

  function finished() {
    clearInterval(timer);
  }

  function toggleNotes() {
    notes = notesBox.checked;
    update();
  }

  async function toggleWorktrees() {
    busy = true;
    update();
    const result = await post("/api/delete/worktrees", { plan_id: plan.plan_id, include: treesBox.checked });
    busy = false;
    if (!result || !result.ok) {
      finished();
      closeDialog();
      if (result) say(result.message, true);
      return;
    }
    worktrees = result.include;
    restarted = false;
    setCountdown(result.seconds_left);
  }

  async function confirmDelete() {
    if (busy || plan.nothing) return;
    busy = true;
    foot.textContent = removal ? "Removing…" : "Deleting…";
    update();
    const result = await post("/api/delete/confirm", { plan_id: plan.plan_id, notes, worktrees });
    busy = false;
    if (result && result.restarted) {
      restarted = true;
      setCountdown(result.seconds_left);
      return;
    }
    finished();
    closeDialog();
    if (!result) return;
    if (!result.ok) {
      say(result.message, true);
      return;
    }
    result.deleted.forEach((id) => marked.delete(id));
    cleanupMarked.clear();
    if (result.lines.length) {
      const lines = [result.summary, ...result.lines].filter(Boolean);
      openDialog(`${verb} finished`, lines.map((line) => el("p", { textContent: line })), [dialogButton("OK", null, true)], closeDialog, null, () => {});
    } else {
      say(result.summary);
    }
    reloadScreen();
  }

  notesBox.addEventListener("change", toggleNotes);
  treesBox.addEventListener("change", toggleWorktrees);

  const keys = {
    y: confirmDelete,
    Delete: () => {
      if (secondsLeft() === 0 && Date.now() - delPressed < DEL_PAUSE_MS) {
        delPressed = Date.now();
        foot.dataset.note = "keep";
        foot.textContent = "Too quick. Wait a moment, then press Del again.";
        return;
      }
      foot.dataset.note = "";
      confirmDelete();
    },
  };
  if (plan.notes.length) keys.m = () => { notesBox.checked = !notesBox.checked; toggleNotes(); };
  if (plan.worktree_count) keys.w = () => { treesBox.checked = !treesBox.checked; toggleWorktrees(); };

  function giveUp() {
    finished();
    post("/api/delete/cancel", { plan_id: plan.plan_id });
  }

  const buttons = [cancel];
  if (plan.rm_commands) {
    const copyCommands = () => { closingDialog = true; $("dialog").close(); giveUp(); copy(plan.rm_commands); };
    keys.c = copyCommands;
    buttons.push(dialogButton("Copy claude rm commands", copyCommands));
  }
  if (!plan.nothing) buttons.push(deleteBtn);

  openDialog(removal ? "Remove worktrees?" : "Delete permanently?", body, buttons, () => {
    // A second click of a double-click that opened this isn't an answer.
    if (Date.now() - opened >= DOUBLE_CLICK_MS) confirmDelete();
  }, keys, () => {
    giveUp();
    say(`Cancelled. Nothing was ${removal ? "removed" : "deleted"}.`);
  });
  setCountdown(plan.seconds_left);
  cancel.focus();
}

function showScreen(name) {
  screen = name;
  screenRequest++;
  for (const each of ["conversations", "worktrees", "cleanup"]) {
    $(`screen-${each}`).hidden = each !== name;
    $(`go-${each}`).setAttribute("aria-pressed", String(each === name));
  }
  $("tools").hidden = name !== "conversations";
}

function reloadScreen() {
  if (screen === "worktrees") loadWorktrees();
  else if (screen === "cleanup") loadCleanup();
  else loadList();
}

function goConversations() {
  showScreen("conversations");
  drawMarks();
  loadList();
}

function goWorktrees(idle = false) {
  onlyIdle = idle;
  if (idle) {
    wtSelected = null;
    wtIndex = 0;
  }
  drawnWt = "";
  showScreen("worktrees");
  $("summary").textContent = "";
  loadWorktrees(true);
}

function goCleanup() {
  // Marks last only for one visit, as the lines may hold other things by the next.
  cleanupMarked.clear();
  drawnCleanup = "";
  showScreen("cleanup");
  $("summary").textContent = "";
  loadCleanup(true);
}

function wtItems() {
  return wtRows.filter((row) => row.id !== undefined);
}

function wtRowOf(id) {
  return id === null ? null : $("wt-rows").querySelector(`tr[data-id="${CSS.escape(id)}"]`);
}

async function loadWorktrees(fresh = false) {
  const request = ++screenRequest;
  const params = new URLSearchParams();
  if (fresh) params.set("fresh", "1");
  if (onlyIdle) params.set("idle", "1");
  const data = await api("/api/worktrees?" + params);
  if (!data || request !== screenRequest || screen !== "worktrees") return;
  wtRows = data.rows;
  wtSummary = data.summary;
  const state = JSON.stringify(wtRows);
  if (state !== drawnWt) {
    drawnWt = state;
    const list = $("wt-list");
    const top = list.scrollTop;
    $("wt-rows").replaceChildren(...wtRows.map(drawWtRow));
    list.scrollTop = top;
  }
  const empty = wtRows.length ? "" : data.empty;
  $("wt-empty").textContent = empty;
  $("wt-empty").hidden = !empty;
  const all = wtItems();
  if (!all.some((row) => row.id === wtSelected)) {
    wtSelected = all.length ? all[Math.min(wtIndex, all.length - 1)].id : null;
  }
  const present = new Set(all.map((row) => row.id));
  if (!onlyIdle) [...wtMarked].filter((id) => !present.has(id)).forEach((id) => wtMarked.delete(id));
  drawWtSelection();
}

function drawWtRow(row) {
  if (row.header !== undefined) {
    return el("tr", { className: "group" }, el("td", { colSpan: 7, textContent: row.header }, el("span", { className: "count", textContent: row.count })));
  }
  const tr = el("tr", { className: row.class });
  tr.dataset.id = row.id;
  tr.append(el("td", { className: "col-mark" }, el("input", { type: "checkbox", tabIndex: -1, ariaLabel: `Mark ${row.cells.folder}` })));
  const cell = (key, name) => el("td", { className: `wt-${name}`, title: row.cells[key] });
  const folder = cell("folder", "folder");
  folder.append(el("bdi", { textContent: row.cells.folder }));
  tr.append(folder);
  for (const [key, name] of [["branch", "branch"], ["changes", "changes"], ["unpushed", "unpushed"], ["conversations", "convs"], ["status", "status"]]) {
    const td = cell(key, name);
    td.textContent = row.cells[key];
    tr.append(td);
  }
  return tr;
}

function wtMarkedShown() {
  return wtItems().filter((row) => wtMarked.has(row.id));
}

function drawWtSelection() {
  wtIndex = Math.max(0, wtItems().findIndex((row) => row.id === wtSelected));
  for (const tr of $("wt-rows").querySelectorAll("tr[data-id]")) {
    const on = wtMarked.has(tr.dataset.id);
    const picked = tr.dataset.id === wtSelected;
    tr.classList.toggle("marked", on);
    tr.classList.toggle("selected", picked);
    if (picked) tr.setAttribute("aria-selected", "true");
    else tr.removeAttribute("aria-selected");
    tr.querySelector(".col-mark input").checked = on;
  }
  const count = wtMarkedShown().length;
  $("summary").textContent = wtSummary + (count ? `  ·  ${count} marked` : "");
  drawWtDetails();
}

function drawWtDetails() {
  const row = wtItems().find((each) => each.id === wtSelected) || null;
  const count = wtMarkedShown().length;
  const state = JSON.stringify([row, count, onlyIdle]);
  if (state === drawnWtDetails) return;
  drawnWtDetails = state;
  const actions = el("div", { className: "actions" },
    actionButton(count ? `Remove ${count}` : "Remove", "d", startRemove, Boolean(row) || count > 0),
    actionButton("All", "a", markAllWorktrees));
  if (onlyIdle) actions.append(actionButton("Show all worktrees", "c", () => goWorktrees(false)));
  if (!row) {
    $("wt-details").replaceChildren(actions);
    $("wt-worked").replaceChildren();
    return;
  }
  const facts = el("dl");
  fact(facts, "Folder", row.path, "mono");
  fact(facts, "Branch", row.branch);
  fact(facts, "Repository", row.repo, "mono");
  const lines = el("div", { className: "state-lines" }, ...row.state.map((line) => el("p", { textContent: line })));
  $("wt-details").replaceChildren(el("h2", { textContent: row.cells.folder }), actions, facts, lines);
  const title = el("p", { className: "older-title", textContent: "Conversations that worked here" });
  const worked = row.worked_here.length
    ? el("ul", { className: "worked" }, ...row.worked_here.map((c) => el("li", { className: c.state },
        el("span", { textContent: c.state ? `${c.name}  (${c.state})` : c.name }), el("small", { textContent: c.notes }))))
    : el("p", { textContent: "None that ccm knows of." });
  $("wt-worked").replaceChildren(title, worked);
}

function selectWorktree(id) {
  wtSelected = id;
  drawWtSelection();
  wtRowOf(id)?.scrollIntoView({ block: "nearest" });
}

function moveWorktree(delta) {
  const all = wtItems();
  if (!all.length) return;
  const at = all.findIndex((row) => row.id === wtSelected);
  selectWorktree(all[Math.min(all.length - 1, Math.max(0, (at < 0 ? 0 : at) + delta))].id);
}

function toggleWtMark(id) {
  if (!wtMarked.delete(id)) wtMarked.add(id);
  drawWtSelection();
}

function markWorktreeAndMove() {
  if (wtSelected === null) return;
  toggleWtMark(wtSelected);
  moveWorktree(1);
}

function markAllWorktrees() {
  const shown = wtItems().map((row) => row.id);
  if (shown.every((id) => wtMarked.has(id))) wtMarked.clear();
  else shown.forEach((id) => wtMarked.add(id));
  drawWtSelection();
}

function startRemove() {
  const shown = wtMarkedShown().map((row) => row.id);
  const paths = shown.length ? shown : wtSelected === null ? [] : [wtSelected];
  if (paths.length) return askPlan("/api/worktrees/plan", { paths });
}

async function loadCleanup(fresh = false) {
  const request = ++screenRequest;
  const data = await api("/api/cleanup" + (fresh ? "?fresh=1" : ""));
  if (!data || request !== screenRequest || screen !== "cleanup") return;
  cleanupLines = data.lines;
  cleanupSummary = data.summary;
  const state = JSON.stringify(cleanupLines.map((line) => [line.key, line.label, line.items, line.frees, line.count]));
  if (state !== drawnCleanup) {
    drawnCleanup = state;
    $("cleanup-rows").replaceChildren(...cleanupLines.map(drawCleanupRow));
  }
  drawCleanupState(data.safety);
}

function drawCleanupRow(line) {
  const tr = el("tr", { className: line.count === 0 ? "is-dim" : "" });
  tr.dataset.key = line.key;
  tr.append(el("td", { className: "col-mark" }, el("input", { type: "checkbox", tabIndex: -1, ariaLabel: `Mark ${line.label}` })),
    el("td", { textContent: line.label }), el("td", { className: "clean-num", textContent: line.items }), el("td", { className: "clean-num", textContent: line.frees }));
  return tr;
}

function cleanupTargets() {
  const marked = cleanupLines.filter((line) => cleanupMarked.has(line.key));
  return marked.length ? marked : cleanupLines.slice(cleanupRow, cleanupRow + 1);
}

let cleanupSafety = "";

function drawCleanupState(safety = cleanupSafety) {
  cleanupSafety = safety;
  $("cleanup-rows").querySelectorAll("tr").forEach((tr, i) => {
    const on = cleanupMarked.has(tr.dataset.key);
    tr.classList.toggle("marked", on);
    tr.classList.toggle("selected", i === cleanupRow);
    if (i === cleanupRow) tr.setAttribute("aria-selected", "true");
    else tr.removeAttribute("aria-selected");
    tr.querySelector(".col-mark input").checked = on;
  });
  const marked = cleanupLines.filter((line) => cleanupMarked.has(line.key)).length;
  $("summary").textContent = cleanupSummary + (marked ? `  ·  ${marked} line${marked === 1 ? "" : "s"} marked` : "");
  const line = cleanupLines[cleanupRow];
  const canDelete = cleanupTargets().some((each) => each.count > 0);
  const state = JSON.stringify([line, canDelete, marked, safety]);
  if (state === drawnAbout || !line) return;
  drawnAbout = state;
  $("cleanup-actions").replaceChildren(
    actionButton("Open", "Enter", openCleanupLine, true, true),
    actionButton(marked ? `Delete ${marked} line${marked === 1 ? "" : "s"}` : "Delete", "d", startCleanupDelete, canDelete),
    actionButton("Mark", "Space", toggleCleanupMark),
    actionButton("All", "a", markAllCleanup));
  const parts = line.about.map((text) => el("p", { textContent: text }));
  if (line.jobs && line.jobs.length) parts.push(jobList(line.jobs));
  parts.push(el("p", { className: "safety", textContent: safety }));
  $("cleanup-about").replaceChildren(...parts);
}

function moveCleanup(to) {
  cleanupRow = Math.min(cleanupLines.length - 1, Math.max(0, to));
  drawCleanupState();
}

function toggleCleanupMark() {
  const line = cleanupLines[cleanupRow];
  if (!line) return;
  if (!cleanupMarked.delete(line.key)) cleanupMarked.add(line.key);
  drawCleanupState();
}

function markAllCleanup() {
  const full = cleanupLines.filter((line) => line.count > 0).map((line) => line.key);
  if (full.length && full.every((key) => cleanupMarked.has(key))) cleanupMarked.clear();
  else {
    cleanupMarked.clear();
    full.forEach((key) => cleanupMarked.add(key));
  }
  drawCleanupState();
}

function startCleanupDelete() {
  return askPlan("/api/cleanup/plan", { lines: cleanupTargets().map((line) => line.key) });
}

function openCleanupLine() {
  const line = cleanupLines[cleanupRow];
  if (!line) return;
  if (line.key === "worktrees") {
    goWorktrees(true);
  } else if (line.key === "jobs") {
    askPlan("/api/cleanup/plan", { lines: ["jobs"], list: true });
  } else {
    view.filters = new Set(line.filters);
    view.tool = null;
    $("search").value = "";
    view.q = "";
    selected = null;
    selectedIndex = 0;
    goConversations();
  }
}

function backFromMarks(marks, back) {
  if (marks.size) {
    marks.clear();
    if (screen === "worktrees") drawWtSelection();
    else drawCleanupState();
  } else {
    back();
  }
}

function sortBy(column) {
  if (view.sort === column.key) {
    view.desc = !view.desc;
  } else {
    view.sort = column.key;
    view.desc = column.descending;
  }
  loadList();
}

function nextSort() {
  const sortable = columns.filter((column) => column.key !== "tool");
  const at = sortable.findIndex((column) => column.key === view.sort);
  sortBy(sortable[(at + 1) % sortable.length]);
}

function reverse() {
  view.desc = !view.desc;
  loadList();
}

function toggleFilter(name) {
  if (!view.filters.delete(name)) view.filters.add(name);
  loadList();
}

function nextTool() {
  if (tools.length < 2) return;
  const at = tools.findIndex((t) => t.key === view.tool);
  view.tool = at === tools.length - 1 ? null : tools[at + 1].key;
  loadList();
}

function nextGroup() {
  view.group = GROUPS[(GROUPS.indexOf(view.group) + 1) % GROUPS.length];
  view.fresh = view.group === "repo";
  loadList();
}

function clearFilters() {
  view.filters.clear();
  view.tool = null;
  loadList();
}

function setQuery(text) {
  view.q = text;
  clearTimeout(searchTimer);
  searchTimer = setTimeout(loadList, SEARCH_WAIT_MS);
}

function clearSearch() {
  $("search").value = "";
  setQuery("");
}

function scrollPreview(lines) {
  $("preview").scrollTop += lines * 20;
}

function onSearchKey(event) {
  if (event.key === "Escape") {
    clearSearch();
    $("search").blur();
  } else if (event.key === "Enter" || event.key === "ArrowDown") {
    event.preventDefault();
    $("search").blur();
  }
}

const KEYS = {
  ArrowDown: () => move(1),
  j: () => move(1),
  ArrowUp: () => move(-1),
  k: () => move(-1),
  PageDown: () => move(PAGE_ROWS),
  PageUp: () => move(-PAGE_ROWS),
  Home: () => move(-Infinity),
  End: () => move(Infinity),
  "/": () => $("search").focus(),
  Escape: clearSearch,
  "[": () => scrollPreview(-3),
  "]": () => scrollPreview(3),
  o: () => toggleFilter("open"),
  x: () => toggleFilter("crashed"),
  J: () => toggleFilter("junk"),
  m: () => toggleFilter("missing"),
  l: () => toggleFilter("large"),
  t: nextTool,
  c: clearFilters,
  Enter: () => resume(),
  f: () => resume(true),
  R: startRename,
  p: togglePin,
  T: startTags,
  y: () => { if (current) copy(current.id); },
  Y: () => { if (current) copy(current.command); },
  O: startReopen,
  F: toggleFocus,
  d: startDelete,
  Delete: startDelete,
  " ": markAndMove,
  a: markAll,
  g: nextGroup,
  s: nextSort,
  r: reverse,
  w: () => goWorktrees(),
  C: goCleanup,
};

const WORKTREE_KEYS = {
  ArrowDown: () => moveWorktree(1),
  j: () => moveWorktree(1),
  ArrowUp: () => moveWorktree(-1),
  k: () => moveWorktree(-1),
  PageDown: () => moveWorktree(PAGE_ROWS),
  PageUp: () => moveWorktree(-PAGE_ROWS),
  Home: () => moveWorktree(-Infinity),
  End: () => moveWorktree(Infinity),
  d: startRemove,
  Delete: startRemove,
  " ": markWorktreeAndMove,
  a: markAllWorktrees,
  c: () => { if (onlyIdle) goWorktrees(false); },
  Escape: () => backFromMarks(wtMarked, goConversations),
  w: goConversations,
  C: goCleanup,
};

const CLEANUP_KEYS = {
  ArrowDown: () => moveCleanup(cleanupRow + 1),
  j: () => moveCleanup(cleanupRow + 1),
  ArrowUp: () => moveCleanup(cleanupRow - 1),
  k: () => moveCleanup(cleanupRow - 1),
  Home: () => moveCleanup(0),
  PageUp: () => moveCleanup(0),
  End: () => moveCleanup(cleanupLines.length - 1),
  PageDown: () => moveCleanup(cleanupLines.length - 1),
  Enter: openCleanupLine,
  d: startCleanupDelete,
  Delete: startCleanupDelete,
  " ": toggleCleanupMark,
  a: markAllCleanup,
  Escape: () => backFromMarks(cleanupMarked, goConversations),
  C: goConversations,
  w: () => goWorktrees(),
};

document.addEventListener("keydown", (event) => {
  if (event.ctrlKey || event.metaKey || event.altKey) return;
  if (planning) {
    event.preventDefault();
    return;
  }
  if ($("dialog").open) {
    const action = dialogKeys && dialogKeys[event.key];
    if (action && !event.repeat && !event.target.matches('input[type="text"]')) {
      event.preventDefault();
      action();
    }
    return;
  }
  if (event.repeat && (event.key === "d" || event.key === "Delete")) return;
  if ((event.key === "Enter" || event.key === " ") && event.target.closest("button")) return;
  if (event.target === $("search")) {
    onSearchKey(event);
    return;
  }
  const action = (screen === "worktrees" ? WORKTREE_KEYS : screen === "cleanup" ? CLEANUP_KEYS : KEYS)[event.key];
  if (action) {
    event.preventDefault();
    action();
  }
});

$("search").addEventListener("input", (event) => setQuery(event.target.value));
// The box gives up the keyboard straight away, so the list's keys keep working.
$("focus-new").addEventListener("change", (event) => { event.target.blur(); toggleFocus(); });
$("rows").addEventListener("click", (event) => {
  const tr = event.target.closest("tr[data-id]");
  if (!tr) return;
  if (event.target.matches(".col-mark input")) toggleMark(tr.dataset.id);
  else if (event.target.matches(".label")) searchTag(event.target.textContent.slice(1));
  else select(tr.dataset.id);
});
$("rows").addEventListener("dblclick", (event) => {
  const tr = event.target.closest("tr[data-id]");
  if (tr && !event.target.matches(".col-mark input, .label")) resume(false, false, tr.dataset.id);
});
$("dialog-form").addEventListener("submit", (event) => {
  event.preventDefault();
  if (dialogSubmit) dialogSubmit();
});
// Closed by anything but this page, which is Esc. The browser doesn't always say "cancel" first.
$("dialog").addEventListener("close", () => {
  if (!closingDialog && dialogCancel) dialogCancel();
  closingDialog = false;
});
// After a mouse click, keys should go back to the list rather than repeat the button.
document.addEventListener("click", (event) => {
  if (event.detail > 0 && !$("dialog").open) event.target.closest("button")?.blur();
});
$("status-copy").addEventListener("click", () => copy($("status-copy").dataset.command));
for (const chip of document.querySelectorAll("[data-filter]")) {
  chip.addEventListener("click", () => toggleFilter(chip.dataset.filter));
}
$("tool").addEventListener("click", nextTool);
$("group").addEventListener("click", nextGroup);
$("reverse").addEventListener("click", reverse);
$("clear").addEventListener("click", clearFilters);
$("reopen").addEventListener("click", startReopen);
$("mark-all").addEventListener("click", markAll);
$("go-conversations").addEventListener("click", goConversations);
$("go-worktrees").addEventListener("click", () => goWorktrees());
$("go-cleanup").addEventListener("click", goCleanup);
$("wt-rows").addEventListener("click", (event) => {
  const tr = event.target.closest("tr[data-id]");
  if (!tr) return;
  if (event.target.matches(".col-mark input")) toggleWtMark(tr.dataset.id);
  else selectWorktree(tr.dataset.id);
});
$("cleanup-rows").addEventListener("click", (event) => {
  const tr = event.target.closest("tr[data-key]");
  if (!tr) return;
  cleanupRow = [...tr.parentNode.children].indexOf(tr);
  if (event.target.matches(".col-mark input")) toggleCleanupMark();
  else drawCleanupState();
});
$("cleanup-rows").addEventListener("dblclick", (event) => {
  if (event.target.closest("tr[data-key]") && !event.target.matches(".col-mark input")) openCleanupLine();
});

async function poll() {
  if (document.hidden) return;
  if (screen !== "conversations") {
    reloadScreen();
    return;
  }
  const changes = focusChanges;
  const data = await api("/api/version");
  if (data && changes === focusChanges) drawFocus(data.focus);
  if (data && data.version !== version) loadList();
}

document.addEventListener("visibilitychange", poll);
setInterval(poll, POLL_MS);
// Keeps the "active" ages current while nothing else changes.
setInterval(() => { if (!document.hidden && screen === "conversations") loadList(); }, AGES_MS);
loadList();
