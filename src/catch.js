const T = window.__TAURI__;
const invoke = T.core.invoke;
const cap = document.getElementById("cap");
const glyph = document.getElementById("glyph");
const go = document.getElementById("go");
let timer = 0;
let busy = false;

function theme() {
  let t = null;
  try {
    t = localStorage.getItem("ddd-theme");
  } catch (e) {}
  document.documentElement.setAttribute("data-theme", t || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"));
}

function hide(delay) {
  clearTimeout(timer);
  timer = setTimeout(() => {
    cap.classList.remove("is-on", "run");
    setTimeout(() => invoke("catch_hide"), 320);
  }, delay);
}

function arm() {
  clearTimeout(timer);
  timer = setTimeout(() => hide(0), 6000);
}

T.event.listen("catch", ({ payload: c }) => {
  theme();
  busy = false;
  cap.classList.remove("taken", "run");
  glyph.setAttribute("href", `#g-${c.kind}`);
  go.setAttribute("href", "#g-arrow");
  document.getElementById("title").textContent = c.title;
  document.getElementById("detail").textContent = [c.detail, c.host].filter(Boolean).join(" · ");
  void cap.offsetWidth;
  cap.classList.add("is-on", "run");
  arm();
});

cap.addEventListener("mouseenter", () => clearTimeout(timer));
cap.addEventListener("mouseleave", () => !busy && hide(2500));
cap.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  hide(0);
});

cap.addEventListener("click", async () => {
  if (busy) return;
  busy = true;
  clearTimeout(timer);
  try {
    const r = await invoke("clip_take");
    if (r.type === "added" || r.type === "duplicate") {
      cap.classList.add("taken");
      glyph.setAttribute("href", "#g-check");
      go.setAttribute("href", "#g-check");
      hide(900);
    } else {
      hide(0);
    }
  } catch (e) {
    hide(0);
  }
});
