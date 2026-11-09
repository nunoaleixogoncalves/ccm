"use strict";

// Conversations are untrusted text. Everything here goes through textContent, never as markup.

const POLL_MS = 2000;
const AGES_MS = 30000;
const SEARCH_WAIT_MS = 120;
const DETAILS_WAIT_MS = 60;
const PAGE_ROWS = 10;
const FORK_WAIT_MS = 15 * 60 * 1000;
const MAX_NAME = 200;
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
let listRequest = 0;
let detailsRequest = 0;
let detailsTimer = 0;
let searchTimer = 0;
let shownDetails = null;
// What the detail pane shows, for the actions on it.
let current = null;
// A fork just started, to select once its tool saves it.
let awaitingFork = null;

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
  const data = await api(address());
  if (!data || request !== listRequest) return;
  version = data.version;
  columns = data.columns;
  tools = data.tools;
  rows = data.rows;
  drawSummary(data);
  drawHead();
  drawRows();
  drawChips();
  drawEmpty(data);
  keepSelection();
  loadDetails();
}

function drawSummary(data) {
  const parts = [`${data.total} conversations`, `${data.total_size} on disk`, `${data.open} open`];
  if (data.filtered) parts.push(`showing ${data.shown}`);
  if (data.crashed) parts.push(`${data.crashed} crashed`);
  if (data.loading) parts.push("scanning…");
  if (data.notice) parts.push(data.notice);
  $("summary").textContent = parts.join("  ·  ");
}

function drawHead() {
  const state = JSON.stringify([columns, view.sort, view.desc]);
  if (state === drawnColumns) return;
  drawnColumns = state;
  $("head").replaceChildren(...columns.map((column) => {
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
    return el("tr", { className: "group" }, el("td", { colSpan: columns.length, textContent: row.header }, count));
  }
  const tr = el("tr", { className: rowClass(row.cells) });
  tr.dataset.id = row.id;
  for (const column of columns) {
    const td = el("td", { className: "col-" + column.key, title: row.cells[column.key] });
    if (column.key === "folder") {
      td.append(el("bdi", { textContent: row.cells.folder }));
    } else {
      td.textContent = row.cells[column.key];
    }
    if (column.key === "name" && row.tag) {
      td.append(el("span", { className: "tag", textContent: row.tag }));
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
  const actions = el("div", { className: "actions" },
    actionButton("Resume", "Enter", () => resume(), data.can.resume, true),
    actionButton("Fork", "f", () => resume(true), data.can.fork),
    actionButton("Rename", "R", startRename, data.can.rename),
    actionButton("Copy ID", "y", () => copy(data.id)),
    actionButton("Copy command", "Y", () => copy(data.command)));
  details.replaceChildren(el("h2", { textContent: data.name }), actions, facts);
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

function openDialog(title, body, buttons, onSubmit) {
  $("dialog-title").textContent = title;
  $("dialog-body").replaceChildren(...body);
  $("dialog-buttons").replaceChildren(...buttons);
  dialogSubmit = onSubmit;
  $("dialog").showModal();
}

function closeDialog() {
  $("dialog").close();
}

function dialogButton(label, action, primary = false) {
  const button = el("button", { type: primary ? "submit" : "button", className: primary ? "chip main" : "chip", textContent: label });
  if (!primary) button.addEventListener("click", action);
  return button;
}

function cancelButton() {
  return dialogButton("Cancel", () => { closeDialog(); say("Cancelled."); });
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
  y: () => { if (current) copy(current.id); },
  Y: () => { if (current) copy(current.command); },
  O: startReopen,
  g: nextGroup,
  s: nextSort,
  r: reverse,
};

document.addEventListener("keydown", (event) => {
  if (event.ctrlKey || event.metaKey || event.altKey) return;
  if ($("dialog").open) return;
  if (event.key === "Enter" && event.target.closest("button")) return;
  if (event.target === $("search")) {
    onSearchKey(event);
    return;
  }
  const action = KEYS[event.key];
  if (action) {
    event.preventDefault();
    action();
  }
});

$("search").addEventListener("input", (event) => setQuery(event.target.value));
$("rows").addEventListener("click", (event) => {
  const tr = event.target.closest("tr[data-id]");
  if (tr) select(tr.dataset.id);
});
$("rows").addEventListener("dblclick", (event) => {
  const tr = event.target.closest("tr[data-id]");
  if (tr) resume(false, false, tr.dataset.id);
});
$("dialog-form").addEventListener("submit", (event) => {
  event.preventDefault();
  if (dialogSubmit) dialogSubmit();
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

async function poll() {
  if (document.hidden) return;
  const data = await api("/api/version");
  if (data && data.version !== version) loadList();
}

document.addEventListener("visibilitychange", poll);
setInterval(poll, POLL_MS);
// Keeps the "active" ages current while nothing else changes.
setInterval(() => { if (!document.hidden) loadList(); }, AGES_MS);
loadList();
