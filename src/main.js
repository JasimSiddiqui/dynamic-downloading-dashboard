const T = window.__TAURI__;
const invoke = T.core.invoke;
const listen = T.event.listen;
const win = T.window.getCurrentWindow();

const $ = (s, r = document) => r.querySelector(s);
const el = (tag, cls, html) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (html != null) n.innerHTML = html;
  return n;
};
const icon = (id, cls = "") => `<svg class="${cls}"><use href="#i-${id}"/></svg>`;
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

const stage = $("#stage");
const list = $("#list");
const where = $("#where");
const q = $("#q");
const note = $("#note");
const chip = $("#chip");
const sheet = $("#sheet");
const sheetBody = $("#sheet-body");
const scrim = $("#scrim");
const menu = $("#menu");
const toast = $("#toast");
const tpl = $("#row-tpl");

const items = new Map();
const ticks = new Map();
const rows = new Map();
const history = [];
let settings = null;
let version = "";
let selected = null;
let total = 0;

const units = ["B", "KB", "MB", "GB", "TB"];
function bytes(n) {
  if (n == null) return "";
  let v = n;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return i === 0 ? `${n} B` : `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}

function rate(bps) {
  if (bps >= 1e9) return [(bps / 1e9).toFixed(2), "GB/s"];
  if (bps >= 1e6) return [(bps / 1e6).toFixed(bps >= 1e8 ? 0 : 1), "MB/s"];
  return [(bps / 1e3).toFixed(0), "KB/s"];
}

function eta(s) {
  if (!isFinite(s) || s <= 0) return "";
  if (s < 60) return `${Math.ceil(s)} s`;
  if (s < 3600) return `${Math.round(s / 60)} min`;
  const h = Math.floor(s / 3600);
  const m = Math.round((s % 3600) / 60);
  return m ? `${h} h ${m} min` : `${h} h`;
}

function when(ms) {
  if (!ms) return "";
  const d = new Date(ms);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}

const groups = {
  video: ["mp4", "mkv", "webm", "mov", "avi", "m4v", "wmv", "flv", "ts", "m3u8", "mpd"],
  audio: ["mp3", "flac", "wav", "m4a", "aac", "ogg", "opus", "wma"],
  archive: ["zip", "rar", "7z", "tar", "gz", "tgz", "xz", "bz2", "zst", "iso", "img", "dmg"],
  document: ["pdf", "epub", "mobi", "azw3", "djvu", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "txt", "csv", "cbz", "cbr", "fb2"],
  image: ["jpg", "jpeg", "png", "gif", "webp", "avif", "bmp", "tif", "tiff", "svg", "heic", "psd"],
  app: ["exe", "msi", "msix", "apk", "appimage", "deb", "rpm", "pkg", "jar"],
  torrent: ["torrent"],
};

function kindGlyph(v) {
  if (v.kind === "torrent") return "torrent";
  if (v.kind === "video" || v.kind === "stream") return "video";
  if (v.kind === "ftp") return "ftp";
  const ext = (v.name.split(".").pop() || "").toLowerCase();
  for (const [g, exts] of Object.entries(groups)) if (exts.includes(ext)) return g;
  return "file";
}

function actGlyph(s) {
  return { running: "pause", queued: "pause", seeding: "pause", paused: "play", failed: "retry", expired: "retry", done: "open" }[s] || "play";
}

function setUse(svg, id) {
  svg.querySelector("use").setAttribute("href", `#i-${id}`);
}

function sorted() {
  return [...items.values()].sort((a, b) => b.created - a.created);
}

function refreshEmpty() {
  const empty = items.size === 0;
  stage.classList.toggle("is-empty", empty);
}

function rowFor(v) {
  let li = rows.get(v.id);
  if (!li) {
    li = tpl.content.firstElementChild.cloneNode(true);
    li.dataset.id = v.id;
    rows.set(v.id, li);
    const next = sorted().find((o) => o.created < v.created && rows.has(o.id) && o.id !== v.id);
    list.insertBefore(li, next ? rows.get(next.id) : null);
  }
  return li;
}

function render(v) {
  items.set(v.id, v);
  const li = rowFor(v);
  li.dataset.status = v.status;
  li.dataset.kind = v.kind;
  setUse($(".row__kind", li), v.status === "done" ? "check" : v.status === "failed" || v.status === "expired" ? "bang" : kindGlyph(v));
  setUse($(".row__act", li), actGlyph(v.status));
  $(".row__name", li).textContent = v.name;
  $(".row__src", li).textContent = v.sources.join("\n");
  $('[data-act="open"]', li).hidden = v.status !== "done";
  $('[data-act="copy"]', li).hidden = !v.sources.length;
  paint(v);
  refreshEmpty();
}

function paint(v) {
  const li = rows.get(v.id);
  if (!li) return;
  const t = ticks.get(v.id);
  const done = t ? t.done : v.done;
  const size = t && t.size ? t.size : v.size;
  const pct = size ? Math.min(100, (done / size) * 100) : 0;
  const meta = $(".row__meta", li);
  const sp = $(".row__speed", li);
  const et = $(".row__eta", li);
  const host = v.host ? ` · ${v.host}` : "";
  li.classList.toggle("is-waiting", !!(t && t.waiting));
  li.classList.toggle("is-preparing", !!(t && t.preparing));
  $(".row__fill", li).style.width = v.status === "done" ? "100%" : `${pct}%`;
  $(".row__line", li).style.width = v.status === "done" ? "100%" : `${pct}%`;
  switch (v.status) {
    case "running": {
      meta.textContent = size ? `${bytes(done)} of ${bytes(size)}${host}` : `${bytes(done)}${host}`;
      if (t && t.speed > 0) {
        const [n, u] = rate(t.speed);
        sp.textContent = `${n} ${u}`;
        et.textContent = size ? eta((size - done) / t.speed) : "";
      } else {
        sp.innerHTML = icon("clock");
        et.textContent = "";
      }
      break;
    }
    case "seeding":
      meta.textContent = `${bytes(size)}${host}`;
      sp.innerHTML = icon("up");
      et.textContent = "";
      break;
    case "queued":
      meta.textContent = size ? `${bytes(size)}${host}` : v.host || "";
      sp.innerHTML = icon("clock");
      et.textContent = "";
      break;
    case "paused":
      meta.textContent = size ? `${bytes(done)} of ${bytes(size)}${host}` : `${bytes(done)}${host}`;
      sp.textContent = size ? `${Math.floor(pct)}%` : "";
      et.textContent = "";
      break;
    case "done":
      meta.textContent = `${bytes(size)}${host}`;
      sp.textContent = "";
      et.textContent = when(v.finished);
      break;
    default:
      meta.textContent = v.error || "";
      sp.textContent = size && done ? `${Math.floor(pct)}%` : "";
      et.textContent = "";
  }
  if (li.classList.contains("is-open")) lanes(li, t);
}

function lanes(li, t) {
  const box = $(".lanes", li);
  const ls = t && t.lanes ? t.lanes.slice(0, 32) : [];
  while (box.children.length > ls.length) box.lastChild.remove();
  while (box.children.length < ls.length) {
    const lane = el("div", "lane");
    lane.append(el("div", "lane__track"), el("div", "lane__done"));
    box.append(lane);
  }
  ls.forEach(([s, p, e], i) => {
    const lane = box.children[i];
    lane.firstChild.style.left = `${s * 100}%`;
    lane.firstChild.style.width = `${Math.max(0.4, (e - s) * 100)}%`;
    lane.lastChild.style.left = `${s * 100}%`;
    lane.lastChild.style.width = `${Math.max(0, (p - s) * 100)}%`;
  });
}

function drop(id) {
  const li = rows.get(id);
  if (li) li.remove();
  rows.delete(id);
  items.delete(id);
  ticks.delete(id);
  if (selected === id) selected = null;
  refreshEmpty();
}

function spark() {
  const w = 300;
  const h = 40;
  const max = Math.max(1, ...history);
  const n = history.length;
  if (n < 2) return;
  const pts = history.map((v, i) => [(i / (n - 1)) * w, h - 3 - (v / max) * (h - 6)]);
  const line = pts.map((p, i) => `${i ? "L" : "M"}${p[0].toFixed(1)},${p[1].toFixed(1)}`).join("");
  $("#spark-line").setAttribute("d", line);
  $("#spark-area").setAttribute("d", `${line}L${w},${h}L${pts[0][0].toFixed(1)},${h}Z`);
}

function onProgress({ items: ts, total: tot }) {
  const seen = new Set();
  for (const t of ts) {
    ticks.set(t.id, t);
    seen.add(t.id);
    const v = items.get(t.id);
    if (v) paint(v);
  }
  for (const id of [...ticks.keys()]) {
    if (!seen.has(id)) {
      ticks.delete(id);
      const v = items.get(id);
      if (v) paint(v);
    }
  }
  total = tot;
  const active = ts.length > 0;
  history.push(tot);
  if (history.length > 120) history.shift();
  const sp = $("#speed");
  sp.classList.toggle("is-on", active);
  if (active) {
    const [n, u] = rate(tot);
    $("#speed-num").textContent = n;
    $("#speed-unit").textContent = u;
    spark();
  } else {
    history.length = 0;
  }
  const mini = $("#mini");
  const scrolled = stage.scrollTop > 140;
  mini.classList.toggle("is-on", active && scrolled);
  if (active) mini.textContent = rate(tot).join(" ");
}

function say(msg) {
  where.classList.remove("shake");
  void where.offsetWidth;
  where.classList.add("shake");
  note.textContent = msg || "";
  note.classList.toggle("is-on", !!msg);
  clearTimeout(say.t);
  say.t = setTimeout(() => note.classList.remove("is-on"), 4500);
}

let toastTimer = 0;
function showToast(text, btn, onBtn) {
  toast.innerHTML = `<span>${esc(text)}</span><button type="button">${esc(btn || "")}</button>`;
  toast.classList.toggle("has-btn", !!btn);
  if (btn) toast.querySelector("button").onclick = () => {
    toast.classList.remove("is-on");
    onBtn && onBtn();
  };
  toast.classList.add("is-on");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.classList.remove("is-on"), 5000);
}

function openSheet(node) {
  sheetBody.replaceChildren(node);
  sheet.classList.add("is-on");
  scrim.classList.add("is-on");
}

function closeSheet() {
  sheet.classList.remove("is-on");
  scrim.classList.remove("is-on");
  sheet.dataset.kind = "";
}

function loadingSheet(text) {
  let host = "";
  try {
    host = new URL(text.trim().split(/\s/)[0]).host.replace(/^www\./, "");
  } catch (e) {}
  const n = el("div", "loading", `<div class="loading__dots"><i></i><i></i><i></i></div><div class="loading__host">${esc(host)}</div>`);
  sheet.dataset.kind = "loading";
  openSheet(n);
}

function busy(on) {
  where.classList.toggle("is-busy", on);
}

async function submit(text) {
  text = (text || "").trim();
  if (!text || where.classList.contains("is-busy")) return;
  busy(true);
  const slow = setTimeout(() => loadingSheet(text), 900);
  try {
    handle(await invoke("submit", { text }));
  } catch (e) {
    if (sheet.dataset.kind === "loading") closeSheet();
    say(String(e));
  } finally {
    clearTimeout(slow);
    busy(false);
  }
}

function clearInput() {
  q.value = "";
  where.classList.remove("has-text");
  note.classList.remove("is-on");
}

function flash(id) {
  const li = rows.get(id);
  if (!li) return;
  li.scrollIntoView({ block: "nearest", behavior: "smooth" });
  li.classList.remove("pulse");
  void li.offsetWidth;
  li.classList.add("pulse");
}

function handle(r) {
  if (sheet.dataset.kind === "loading") closeSheet();
  chip.hidden = true;
  switch (r.type) {
    case "added":
      clearInput();
      break;
    case "duplicate":
      clearInput();
      flash(r.id);
      break;
    case "video":
      videoSheet(r);
      break;
    case "stream":
      streamSheet(r);
      break;
    case "torrent":
      torrentSheet(r);
      break;
    case "links":
      linksSheet(r);
      break;
    case "nothing":
      if (r.reason) say(r.reason);
      break;
  }
}

function pills(options, current, onPick) {
  const box = el("div", "pills");
  let on = current;
  for (const o of options) {
    const b = el("button", "pill" + (o.key === on ? " is-on" : ""), o.html);
    b.type = "button";
    b.onclick = () => {
      on = o.key;
      box.querySelectorAll(".pill").forEach((p) => p.classList.remove("is-on"));
      b.classList.add("is-on");
      onPick(o.key);
    };
    box.append(b);
  }
  return box;
}

function head(art, title, sub) {
  const h = el("div", "sh-head");
  h.innerHTML = `${art}<div><div class="sh-title">${esc(title)}</div><div class="sh-sub">${esc(sub)}</div></div>`;
  return h;
}

function goButton(label, onGo) {
  const b = el("button", "big", `${icon("arrow")}${label ? `<span>${esc(label)}</span>` : ""}`);
  b.type = "button";
  b.onclick = async () => {
    b.disabled = true;
    try {
      await onGo();
      clearInput();
      closeSheet();
    } catch (e) {
      b.disabled = false;
      showToast(String(e));
    }
  };
  return b;
}

function quality(h) {
  return h >= 2160 ? "4K" : h >= 1440 ? "1440" : String(h);
}

function videoSheet({ token, info }) {
  const node = el("div");
  const dur = info.duration ? `${Math.floor(info.duration / 60)}:${String(Math.round(info.duration % 60)).padStart(2, "0")}` : "";
  const art = info.thumb ? `<img class="sh-thumb" src="${esc(info.thumb)}" alt="" referrerpolicy="no-referrer">` : `<div class="sh-badge">${icon("video")}</div>`;
  node.append(head(art, info.title || "Video", [info.site, dur].filter(Boolean).join(" · ")));
  const opts = info.heights.slice(0, 7).map((h) => ({ key: h, html: `<span>${quality(h)}${h < 1440 ? "<small>p</small>" : ""}</span>` }));
  opts.push({ key: "audio", html: icon("headphones") });
  let pick = opts[0].key;
  node.append(pills(opts, pick, (k) => (pick = k)));
  const foot = el("div", "sh-foot");
  foot.append(el("div", "sh-foot__info"), goButton("", () => invoke("add_video", { token, height: pick === "audio" ? null : pick, audio: pick === "audio" })));
  node.append(foot);
  sheet.dataset.kind = "video";
  openSheet(node);
}

function streamSheet({ token, name, choices }) {
  const node = el("div");
  node.append(head(`<div class="sh-badge">${icon("video")}</div>`, name, ""));
  const opts = choices.map((c) => ({ key: c.id, html: c.height ? `<span>${quality(c.height)}${c.height < 1440 ? "<small>p</small>" : ""}</span>` : esc(c.label) }));
  let pick = opts[0] && opts[0].key;
  node.append(pills(opts, pick, (k) => (pick = k)));
  const foot = el("div", "sh-foot");
  foot.append(el("div", "sh-foot__info"), goButton("", () => invoke("add_stream", { token, choice: pick ?? null })));
  node.append(foot);
  sheet.dataset.kind = "stream";
  openSheet(node);
}

function pickList(entries, onChange) {
  const ul = el("ul", "picks");
  const state = entries.map(() => true);
  const boxes = [];
  const sync = () => {
    boxes.forEach((li, i) => li.classList.toggle("is-on", state[i]));
    if (all) all.classList.toggle("is-on", state.every(Boolean));
    onChange(state);
  };
  let all = null;
  if (entries.length > 1) {
    all = el("li", "pick pick--all is-on", `<span class="pick__box">${icon("check")}</span><span></span><span class="pick__name"></span><span class="pick__size"></span>`);
    all.onclick = () => {
      const to = !state.every(Boolean);
      state.fill(to);
      sync();
    };
    ul.append(all);
  }
  entries.forEach((e, i) => {
    const li = el("li", "pick is-on", `<span class="pick__box">${icon("check")}</span>${icon(e.glyph, "pick__ico")}<span class="pick__name" title="${esc(e.name)}">${esc(e.name)}</span><span class="pick__size">${e.size != null ? bytes(e.size) : ""}</span>`);
    li.onclick = () => {
      state[i] = !state[i];
      sync();
    };
    boxes.push(li);
    ul.append(li);
  });
  return { ul, state, boxes, sync };
}

function glyphFor(name) {
  return kindGlyph({ kind: "file", name });
}

function torrentSheet({ token, name, files }) {
  const node = el("div");
  const totalSize = files.reduce((a, f) => a + f.size, 0);
  node.append(head(`<div class="sh-badge">${icon("torrent")}</div>`, name, `${files.length} ${files.length === 1 ? "file" : "files"} · ${bytes(totalSize)}`));
  const info = el("div", "sh-foot__info");
  let chosen = files.map(() => true);
  const pl = pickList(
    files.map((f) => ({ name: f.name, size: f.size, glyph: glyphFor(f.name) })),
    (s) => {
      chosen = s;
      const sum = files.reduce((a, f, i) => a + (s[i] ? f.size : 0), 0);
      info.textContent = bytes(sum);
      go.disabled = !s.some(Boolean);
    }
  );
  if (files.length > 1) node.append(pl.ul);
  const foot = el("div", "sh-foot");
  const go = goButton("", () => {
    const idx = chosen.map((on, i) => (on ? i : -1)).filter((i) => i >= 0);
    return invoke("add_torrent", { token, files: idx.length === files.length ? null : idx });
  });
  foot.append(info, go);
  node.append(foot);
  info.textContent = bytes(totalSize);
  sheet.dataset.kind = "torrent";
  openSheet(node);
}

function linksSheet({ links, headers }) {
  const node = el("div");
  let host = "";
  try {
    host = new URL(links[0].url).host.replace(/^www\./, "");
  } catch (e) {}
  node.append(head(`<div class="sh-badge">${icon("link")}</div>`, `${links.length} ${links.length === 1 ? "file" : "files"}`, host));
  const counts = {};
  for (const l of links) counts[l.group] = (counts[l.group] || 0) + 1;
  const order = ["video", "audio", "document", "archive", "app", "image", "torrent", "other"].filter((g) => counts[g]);
  const glyph = { other: "file" };
  let filter = "all";
  const opts = [{ key: "all", html: `${links.length}` }, ...order.map((g) => ({ key: g, html: `${icon(glyph[g] || g)}<small>${counts[g]}</small>` }))];
  if (order.length > 1) node.append(pills(opts, "all", (k) => ((filter = k), apply())));
  const sizes = new Array(links.length).fill(null);
  let chosen = links.map(() => true);
  const info = el("div", "sh-foot__info");
  const pl = pickList(
    links.map((l) => ({ name: l.name, size: null, glyph: glyph[l.group] || l.group })),
    (s) => {
      chosen = s;
      count();
    }
  );
  node.append(pl.ul);
  const go = goButton("", () => {
    const urls = links.filter((l, i) => chosen[i] && (filter === "all" || l.group === filter)).map((l) => l.url);
    return invoke("add_links", { urls, headers });
  });
  const label = el("span");
  go.append(label);
  function count() {
    const n = links.filter((l, i) => chosen[i] && (filter === "all" || l.group === filter)).length;
    label.textContent = n ? String(n) : "";
    go.disabled = !n;
    const sum = links.reduce((a, l, i) => a + (chosen[i] && (filter === "all" || l.group === filter) && sizes[i] ? sizes[i] : 0), 0);
    info.textContent = sum ? bytes(sum) : "";
  }
  function apply() {
    pl.boxes.forEach((li, i) => (li.style.display = filter === "all" || links[i].group === filter ? "" : "none"));
    count();
  }
  const foot = el("div", "sh-foot");
  foot.append(info, go);
  node.append(foot);
  count();
  sheet.dataset.kind = "links";
  openSheet(node);
  const want = links.slice(0, 80).map((l) => l.url);
  invoke("probe_sizes", { urls: want, headers }).then((res) => {
    res.forEach((s, i) => {
      sizes[i] = s;
      const cell = pl.boxes[i] && pl.boxes[i].querySelector(".pick__size");
      if (cell && s != null) cell.textContent = bytes(s);
    });
    count();
  });
}

const caps = [0, 1e6, 2e6, 5e6, 10e6, 20e6, 50e6, 100e6];
const ratios = [0, 0.5, 1, 2, 3, 5];

function stepper(values, value, show, onSet) {
  const box = el("div", "step");
  let i = Math.max(0, values.indexOf(value));
  if (values.indexOf(value) < 0) i = values.reduce((best, v, k) => (Math.abs(v - value) < Math.abs(values[best] - value) ? k : best), 0);
  const out = el("output", "", show(values[i]));
  const minus = el("button", "", "−");
  const plus = el("button", "", "+");
  minus.type = plus.type = "button";
  minus.onclick = () => {
    if (i > 0) {
      i--;
      out.textContent = show(values[i]);
      onSet(values[i]);
    }
  };
  plus.onclick = () => {
    if (i < values.length - 1) {
      i++;
      out.textContent = show(values[i]);
      onSet(values[i]);
    }
  };
  box.append(minus, out, plus);
  return box;
}

function toggle(on, onSet) {
  const t = el("button", "toggle" + (on ? " is-on" : ""));
  t.type = "button";
  t.onclick = () => {
    on = !on;
    t.classList.toggle("is-on", on);
    onSet(on);
  };
  return t;
}

async function save(patch) {
  settings = await invoke("set_settings", { settings: { ...settings, ...patch } });
}

function settingsSheet() {
  const node = el("div", "set");
  const row = (label, control) => {
    const r = el("div", "set__row");
    r.append(el("div", "set__label", esc(label)), control);
    node.append(r);
  };
  const folder = el("button", "set__value set__value--btn", esc(settings.dir));
  folder.type = "button";
  folder.onclick = async () => {
    const dir = await T.dialog.open({ directory: true, defaultPath: settings.dir });
    if (dir) {
      await save({ dir });
      folder.textContent = settings.dir;
    }
  };
  row("Folder", folder);
  const range = (a, b) => Array.from({ length: b - a + 1 }, (_, k) => a + k);
  row("Connections", stepper(range(1, 32), settings.connections, String, (v) => save({ connections: v })));
  row("At once", stepper(range(1, 10), settings.atOnce, String, (v) => save({ atOnce: v })));
  row("Speed cap", stepper(caps, settings.speedCap, (v) => (v ? rate(v).join(" ") : "∞"), (v) => save({ speedCap: v })));
  row("Seeding", stepper(ratios, settings.seedRatio, (v) => (v ? `${v}×` : "Off"), (v) => save({ seedRatio: v })));
  row("Magnets", toggle(settings.magnets, (v) => save({ magnets: v })));
  row("Catch", toggle(settings.catch, (v) => save({ catch: v })));
  const seg = el("div", "seg");
  for (const [k, label] of [["light", "Light"], ["dark", "Dark"]]) {
    const b = el("button", document.documentElement.dataset.theme === k ? "is-on" : "", label);
    b.type = "button";
    b.onclick = () => {
      setTheme(k);
      seg.querySelectorAll("button").forEach((x) => x.classList.toggle("is-on", x === b));
    };
    seg.append(b);
  }
  row("Theme", seg);
  const foot = el("div", "set__foot", `<span>v${esc(version)}</span><a href="#" id="gh">GitHub</a>`);
  foot.querySelector("#gh").onclick = (e) => {
    e.preventDefault();
    T.opener.openUrl("https://github.com/JasimSiddiqui/dynamic-downloading-dashboard");
  };
  node.append(foot);
  sheet.dataset.kind = "settings";
  openSheet(node);
}

function setTheme(theme) {
  document.documentElement.setAttribute("data-theme", theme);
  try {
    localStorage.setItem("ddd-theme", theme);
  } catch (e) {}
  win.setTheme(theme).catch(() => {});
}

function act(v) {
  switch (v.status) {
    case "running":
    case "queued":
    case "seeding":
      return invoke("pause", { id: v.id });
    case "done":
      return invoke("open", { id: v.id });
    default:
      return invoke("resume", { id: v.id });
  }
}

const pendingRemoval = new Map();
function removeSoft(id, deleteFile) {
  const li = rows.get(id);
  if (!li) return;
  li.classList.add("is-gone");
  const timer = setTimeout(() => {
    pendingRemoval.delete(id);
    invoke("remove", { id, delete: deleteFile });
  }, 5000);
  pendingRemoval.set(id, timer);
  setTimeout(() => {
    if (pendingRemoval.has(id)) li.style.display = "none";
  }, 300);
  showToast(deleteFile ? "Deleted" : "Removed", "Undo", () => {
    clearTimeout(pendingRemoval.get(id));
    pendingRemoval.delete(id);
    li.style.display = "";
    requestAnimationFrame(() => li.classList.remove("is-gone"));
  });
}

async function copyLink(v) {
  const text = v.sources[0];
  if (!text) return;
  await invoke("clip_ignore", { text });
  await navigator.clipboard.writeText(text);
  showToast("Copied");
}

function select(id) {
  if (selected != null && rows.get(selected)) rows.get(selected).classList.remove("is-selected");
  selected = id;
  if (id != null && rows.get(id)) rows.get(id).classList.add("is-selected");
}

list.addEventListener("click", (e) => {
  const li = e.target.closest(".row");
  if (!li) return;
  const v = items.get(Number(li.dataset.id));
  if (!v) return;
  if (e.target.closest(".row__icon")) return act(v);
  if (e.target.closest(".row__folder")) return invoke("reveal", { id: v.id });
  const a = e.target.closest(".act");
  if (a) {
    const kind = a.dataset.act;
    if (kind === "open") invoke("open", { id: v.id });
    if (kind === "reveal") invoke("reveal", { id: v.id });
    if (kind === "copy") copyLink(v);
    if (kind === "remove") removeSoft(v.id, false);
    return;
  }
  if (e.target.closest(".row__src")) return;
  select(v.id);
  li.classList.toggle("is-open");
  paint(v);
});

list.addEventListener("dblclick", (e) => {
  const li = e.target.closest(".row");
  if (!li || e.target.closest("button")) return;
  const v = items.get(Number(li.dataset.id));
  if (v && v.status === "done") {
    li.classList.remove("is-open");
    invoke("open", { id: v.id });
  }
});

list.addEventListener("contextmenu", (e) => {
  const li = e.target.closest(".row");
  if (!li) return;
  e.preventDefault();
  const v = items.get(Number(li.dataset.id));
  if (!v) return;
  select(v.id);
  const entries = [];
  if (v.status === "done") entries.push(["open", "Open", () => invoke("open", { id: v.id })]);
  entries.push(["folder", "Show in folder", () => invoke("reveal", { id: v.id })]);
  if (v.sources.length) entries.push(["copy", "Copy link", () => copyLink(v)]);
  if (["running", "queued", "seeding"].includes(v.status)) entries.push(["pause", "Pause", () => invoke("pause", { id: v.id })]);
  if (["paused", "failed", "expired"].includes(v.status)) entries.push(["play", "Resume", () => invoke("resume", { id: v.id })]);
  entries.push(null);
  entries.push(["x", "Remove", () => removeSoft(v.id, false)]);
  if (v.status === "done" || v.kind === "torrent") entries.push(["trash", "Delete file", () => removeSoft(v.id, true), "danger"]);
  menu.replaceChildren(
    ...entries.map((en) => {
      if (!en) return el("hr");
      const b = el("button", en[3] || "", `${icon(en[0])}<span>${esc(en[1])}</span>`);
      b.type = "button";
      b.onclick = () => {
        hideMenu();
        en[2]();
      };
      return b;
    })
  );
  const w = 200;
  const h = entries.length * 38;
  menu.style.left = `${Math.min(e.clientX, innerWidth - w - 8)}px`;
  menu.style.top = `${Math.min(e.clientY, innerHeight - h - 8)}px`;
  menu.classList.add("is-on");
});

function hideMenu() {
  menu.classList.remove("is-on");
}

document.addEventListener("pointerdown", (e) => {
  if (!e.target.closest(".menu")) hideMenu();
});

scrim.addEventListener("click", closeSheet);

q.addEventListener("input", () => where.classList.toggle("has-text", q.value.trim() !== ""));
q.addEventListener("paste", () => setTimeout(() => submit(q.value), 0));
where.addEventListener("submit", (e) => {
  e.preventDefault();
  submit(q.value);
});

chip.addEventListener("click", async () => {
  chip.hidden = true;
  busy(true);
  const slow = setTimeout(() => loadingSheet(""), 900);
  try {
    handle(await invoke("clip_take"));
  } catch (e) {
    if (sheet.dataset.kind === "loading") closeSheet();
    say(String(e));
  } finally {
    clearTimeout(slow);
    busy(false);
  }
});

const chipGlyph = { file: "file", video: "video", audio: "audio", archive: "archive", document: "document", image: "image", app: "app", torrent: "torrent", many: "link" };
function showChip(c) {
  if (!c) return;
  setUse($(".chip__ico svg", chip), chipGlyph[c.kind] || "file");
  $(".chip__title", chip).textContent = c.title;
  $(".chip__detail", chip).textContent = [c.detail, c.host].filter(Boolean).join(" · ");
  chip.hidden = false;
}

document.addEventListener("keydown", (e) => {
  const inInput = e.target === q;
  if (e.key === "Escape") {
    if (menu.classList.contains("is-on")) return hideMenu();
    if (sheet.classList.contains("is-on")) return closeSheet();
    if (inInput) {
      clearInput();
      q.blur();
    }
    return;
  }
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "v" && !inInput) {
    q.focus();
    return;
  }
  if ((e.ctrlKey || e.metaKey) && e.key === ",") {
    e.preventDefault();
    return settingsSheet();
  }
  if (inInput || sheet.classList.contains("is-on")) return;
  const v = selected != null ? items.get(selected) : null;
  if (!v) return;
  if (e.key === " ") {
    e.preventDefault();
    if (v.status !== "done") act(v);
  } else if (e.key === "Enter") {
    if (v.status === "done") invoke("open", { id: v.id });
  } else if (e.key === "Delete") {
    removeSoft(v.id, e.shiftKey && v.status === "done");
  } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const ids = sorted().map((x) => x.id).filter((id) => !pendingRemoval.has(id));
    const i = ids.indexOf(v.id) + (e.key === "ArrowDown" ? 1 : -1);
    if (ids[i] != null) {
      select(ids[i]);
      rows.get(ids[i]).scrollIntoView({ block: "nearest" });
    }
  }
});

let depth = 0;
const dropEl = $("#drop");
function dragKind(e) {
  const t = e.dataTransfer && [...e.dataTransfer.types];
  return t && (t.includes("Files") || t.includes("text/uri-list") || t.includes("text/plain"));
}
function rowTarget(e) {
  const li = document.elementFromPoint(e.clientX, e.clientY)?.closest(".row");
  if (!li) return null;
  const v = items.get(Number(li.dataset.id));
  return v && ["expired", "failed", "paused"].includes(v.status) && v.kind !== "torrent" ? li : null;
}
window.addEventListener("dragenter", (e) => {
  if (!dragKind(e)) return;
  e.preventDefault();
  depth++;
  dropEl.classList.add("is-on");
});
window.addEventListener("dragover", (e) => {
  if (!dragKind(e)) return;
  e.preventDefault();
  const li = rowTarget(e);
  document.querySelectorAll(".row.drop-target").forEach((r) => r !== li && r.classList.remove("drop-target"));
  if (li) li.classList.add("drop-target");
  dropEl.classList.toggle("is-on", !li);
});
window.addEventListener("dragleave", () => {
  depth = Math.max(0, depth - 1);
  if (!depth) {
    dropEl.classList.remove("is-on");
    document.querySelectorAll(".row.drop-target").forEach((r) => r.classList.remove("drop-target"));
  }
});
window.addEventListener("drop", async (e) => {
  e.preventDefault();
  depth = 0;
  dropEl.classList.remove("is-on");
  const target = rowTarget(e);
  document.querySelectorAll(".row.drop-target").forEach((r) => r.classList.remove("drop-target"));
  const dt = e.dataTransfer;
  const text = dt.getData("text/uri-list") || dt.getData("text/plain");
  if (target && text) {
    try {
      await invoke("relink", { id: Number(target.dataset.id), text });
    } catch (err) {
      showToast(String(err));
    }
    return;
  }
  if (dt.files && dt.files.length) {
    for (const f of dt.files) {
      if (f.name.toLowerCase().endsWith(".torrent")) {
        const data = Array.from(new Uint8Array(await f.arrayBuffer()));
        busy(true);
        try {
          handle(await invoke("submit_bytes", { name: f.name, data }));
        } catch (err) {
          say(String(err));
        } finally {
          busy(false);
        }
      } else if (f.size < 2e6) {
        submit(await f.text());
      }
    }
    return;
  }
  if (text) submit(text);
});

$("#w-min").onclick = () => win.minimize();
$("#w-max").onclick = () => win.toggleMaximize();
$("#w-close").onclick = () => win.close();
$("#theme").onclick = () => setTheme(document.documentElement.dataset.theme === "dark" ? "light" : "dark");
$("#gear").onclick = () => settingsSheet();
$("#folder").onclick = () => invoke("open_folder");
stage.addEventListener("scroll", () => $("#mini").classList.toggle("is-on", stage.scrollTop > 140 && ticks.size > 0), { passive: true });

window.addEventListener("focus", async () => {
  const c = await invoke("clip_peek");
  if (c) showChip(c);
});

listen("item", ({ payload }) => render(payload));
listen("removed", ({ payload }) => drop(payload));
listen("progress", ({ payload }) => onProgress(payload));
listen("clip", ({ payload }) => showChip(payload));
listen("sheet", ({ payload }) => handle(payload));
listen("problem", ({ payload }) => say(String(payload)));

(async () => {
  stage.classList.add("still");
  const b = await invoke("boot");
  settings = b.settings;
  version = b.version;
  for (const v of b.items.sort((a, c) => a.created - c.created)) render(v);
  refreshEmpty();
  const theme = document.documentElement.dataset.theme;
  win.setTheme(theme).catch(() => {});
  requestAnimationFrame(() => {
    win.show();
    setTimeout(() => stage.classList.remove("still"), 600);
  });
  if (!items.size) q.focus();
  const c = await invoke("clip_peek");
  if (c && document.hasFocus()) showChip(c);
})();
