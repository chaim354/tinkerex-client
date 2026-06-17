// Settings surface: file-layer preferences (bandwidth caps + progressive hydration).
// These are persisted per account and applied at the next sign-in/mount (they're
// DaemonConfig start-time options). Selective sync is plan-only (see index.html note).
//
// Tauri commands:
//   get_prefs {serverUrl,email} -> { down_mbps, up_mbps, progressive_hydration }
//   set_prefs {serverUrl,email,prefs}

import { THEMES, THEME_META, applyTheme, currentTheme } from "/theme.js";

const UI = window.MakerUI;
const { invoke, $ } = UI;
const a = () => UI.args();

// ---- theme picker ------------------------------------------------------------

/** Mirror the chosen theme into Prefs.theme (durable; survives a localStorage purge).
 *  Best-effort: if not signed in / IPC fails, localStorage (applyTheme) still holds it. */
async function mirrorThemeToPrefs(name) {
  try {
    const prefs = await invoke("get_prefs", a());
    prefs.theme = name;
    await invoke("set_prefs", { ...a(), prefs });
  } catch {
    /* not signed in or no server — localStorage is the fallback */
  }
}

function selectTheme(name) {
  const applied = applyTheme(name); // sets <html data-theme> + localStorage
  for (const btn of document.querySelectorAll("#themeGrid .theme-swatch")) {
    const on = btn.dataset.theme === applied;
    btn.setAttribute("aria-checked", String(on));
    btn.tabIndex = on ? 0 : -1;
  }
  mirrorThemeToPrefs(applied);
}

function renderThemeGrid() {
  const grid = $("themeGrid");
  if (!grid || grid.childElementCount) return; // render once
  const active = currentTheme();
  THEMES.forEach((name) => {
    const meta = THEME_META[name] || { label: name, note: "", sw: ["#888", "#888", "#888"] };
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "theme-swatch";
    btn.dataset.theme = name;
    btn.setAttribute("role", "radio");
    const on = name === active;
    btn.setAttribute("aria-checked", String(on));
    btn.tabIndex = on ? 0 : -1;

    const prev = document.createElement("span");
    prev.className = "preview";
    prev.style.background = meta.sw[0];
    for (const c of [meta.sw[1], meta.sw[2]]) {
      const dot = document.createElement("i");
      dot.style.background = c;
      prev.appendChild(dot);
    }
    btn.appendChild(prev);

    const lab = document.createElement("span");
    lab.className = "sw-label";
    const nm = document.createElement("span");
    nm.className = "sw-name";
    nm.textContent = meta.label;
    const note = document.createElement("span");
    note.className = "sw-note";
    note.textContent = meta.note;
    lab.appendChild(nm);
    lab.appendChild(note);
    btn.appendChild(lab);

    btn.onclick = () => selectTheme(name);
    grid.appendChild(btn);
  });

  // Arrow-key navigation across the radiogroup (roving tabindex).
  grid.addEventListener("keydown", (e) => {
    const items = [...grid.querySelectorAll(".theme-swatch")];
    const i = items.indexOf(document.activeElement);
    if (i === -1) return;
    let next = null;
    if (e.key === "ArrowRight" || e.key === "ArrowDown") next = items[(i + 1) % items.length];
    else if (e.key === "ArrowLeft" || e.key === "ArrowUp") next = items[(i - 1 + items.length) % items.length];
    else if (e.key === " " || e.key === "Enter") { e.preventDefault(); items[i].click(); return; }
    if (next) {
      e.preventDefault();
      next.focus();
      selectTheme(next.dataset.theme);
    }
  });
}

async function loadPrefs() {
  let prefs;
  // set_prefs/get_prefs take NO device token — they key a file on (serverUrl, email) alone — so
  // signed out these were the one pair that reached a real account's prefs.json on any box, not
  // just a developer's. Don't ask, and refuse the write below.
  if (!UI.signedIn()) {
    $("bwDown").value = 0;
    $("bwUp").value = 0;
    $("progHydrate").checked = false;
    return;
  }
  try {
    prefs = await invoke("get_prefs", a());
  } catch {
    prefs = { down_mbps: 0, up_mbps: 0, progressive_hydration: false };
  }
  $("bwDown").value = prefs.down_mbps ?? 0;
  $("bwUp").value = prefs.up_mbps ?? 0;
  $("progHydrate").checked = !!prefs.progressive_hydration;

  // If localStorage has no theme yet (fresh install / purge) but Prefs does, adopt it
  // as the durable fallback — then localStorage takes over as the fast path.
  let hasLocal = null;
  try {
    hasLocal = localStorage.getItem("maker.theme");
  } catch {
    /* ignore */
  }
  if (!hasLocal && prefs.theme && THEMES.includes(prefs.theme)) {
    selectTheme(prefs.theme);
  }
}

$("prefsSave").onclick = async () => {
  if (!UI.signedIn()) return UI.toast($("settingsToast"), UI.SIGNED_OUT_HINT, "err");
  const prefs = {
    down_mbps: Math.max(0, Number($("bwDown").value) || 0),
    up_mbps: Math.max(0, Number($("bwUp").value) || 0),
    progressive_hydration: $("progHydrate").checked,
  };
  await UI.withBusy($("prefsSave"), "Saving…", async () => {
    try {
      const msg = await invoke("set_prefs", { ...a(), prefs });
      UI.toast($("settingsToast"), msg, "ok");
    } catch (e) {
      UI.toast($("settingsToast"), String(e), "err");
    }
  });
};

window.addEventListener("view:show", (e) => {
  if (e.detail === "settings") {
    renderThemeGrid();
    loadPrefs();
  }
});
