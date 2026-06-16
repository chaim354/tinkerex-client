// Maker client — frontend logic.
//
// Vanilla ES module. All backend calls go through the Tauri `invoke` bridge.
// The command names + argument casing here MUST match src-tauri/src/lib.rs:
//   sync_start / sync_pair / create_pairing / sync_stop / sync_status / platform /
//   browse / file_versions / restore_version / conflicts /
//   resolve_conflict_keep / resolve_conflict_split
// args use camelCase: serverUrl, email, mountpoint, code, nodeId, versionId.

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const val = (id) => $(id).value.trim();

let mode = "new"; // "new" | "pair"

// ---- small helpers -----------------------------------------------------------

/** Turn a raw error (often a Rust String) into something readable. */
const errText = (e) => (e == null ? "Something went wrong" : String(e));
// Escape before interpolating into innerHTML (error text may carry a raw server HTML body).
function escHtml(s){return String(s==null?"":s).replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));}

const fmtSize = (n) =>
  n < 1024
    ? `${n} B`
    : n < 1048576
      ? `${(n / 1024).toFixed(1)} KB`
      : `${(n / 1048576).toFixed(1)} MB`;

/** Absolute timestamp, falling back to the raw string if unparseable. */
const fmtWhen = (s) => {
  const d = new Date(s);
  if (isNaN(d)) return String(s);
  return d.toLocaleString();
};

/** Relative timestamp ("3 min ago"). */
const relWhen = (s) => {
  const d = new Date(s);
  if (isNaN(d)) return "";
  const sec = Math.round((Date.now() - d.getTime()) / 1000);
  if (sec < 60) return "just now";
  const min = Math.round(sec / 60);
  if (min < 60) return `${min} min ago`;
  const hr = Math.round(min / 60);
  if (hr < 24) return `${hr} hr ago`;
  const day = Math.round(hr / 24);
  return `${day} day${day === 1 ? "" : "s"} ago`;
};

/** Dismissible toast. kind: "info" | "ok" | "err". */
const toast = (el, text, kind = "info") => {
  el.className = "toast" + (kind === "err" ? " err" : kind === "ok" ? " ok" : "");
  el.innerHTML = "";
  const msg = document.createElement("span");
  msg.className = "toast-msg";
  msg.textContent = text;
  el.appendChild(msg);
  const x = document.createElement("button");
  x.className = "close";
  x.setAttribute("aria-label", "Dismiss");
  x.textContent = "×";
  x.onclick = () => el.classList.add("hidden");
  el.appendChild(x);
  el.classList.remove("hidden");
};
const hideToast = (el) => el.classList.add("hidden");

/** Run an async action on a button: show a spinner, restore the original label after. */
const withBusy = async (btn, busyLabel, fn) => {
  const original = btn.textContent;
  btn.disabled = true;
  btn.innerHTML = `<span class="spinner"></span>${busyLabel}`;
  try {
    return await fn();
  } finally {
    btn.disabled = false;
    btn.textContent = original;
  }
};

const setFieldError = (input, errEl, msg) => {
  if (msg) {
    input.setAttribute("aria-invalid", "true");
    errEl.textContent = msg;
  } else {
    input.removeAttribute("aria-invalid");
    errEl.textContent = "";
  }
};

const args = () => ({ serverUrl: val("server"), email: val("email") });

// ---- status pill -------------------------------------------------------------

let lastStatus = "unmounted";
const setStatus = (text) => {
  lastStatus = text;
  const pill = $("statusPill");
  const label = $("statusText");
  if (text.startsWith("mounted")) {
    pill.className = "pill on";
    label.textContent = text; // "mounted at <path>"
  } else if (text === "mounting" || text === "pairing") {
    pill.className = "pill busy";
    label.textContent = text === "pairing" ? "pairing…" : "mounting…";
  } else {
    pill.className = "pill";
    label.textContent = "unmounted";
  }
  $("unmountBtn").disabled = !text.startsWith("mounted");
};

const refreshStatus = async () => {
  // don't clobber a transient "mounting"/"pairing" the click handler set
  if (lastStatus === "mounting" || lastStatus === "pairing") return;
  try {
    setStatus(await invoke("sync_status"));
  } catch {
    setStatus("unmounted");
  }
};

// ---- connect tabs ------------------------------------------------------------

const selectTab = (m) => {
  mode = m;
  $("tabNew").setAttribute("aria-selected", String(m === "new"));
  $("tabPair").setAttribute("aria-selected", String(m === "pair"));
  $("pairRow").classList.toggle("hidden", m !== "pair");
  $("mountBtn").textContent = m === "pair" ? "Pair & sync" : "Mount & sync";
};
$("tabNew").onclick = () => selectTab("new");
$("tabPair").onclick = () => selectTab("pair");

// ---- top-level view nav (Sync | Library) -------------------------------------

const showView = (which) => {
  const lib = which === "library";
  $("viewSync").classList.toggle("hidden", lib);
  $("viewLibrary").classList.toggle("hidden", !lib);
  $("navSync").classList.toggle("active", !lib);
  $("navLibrary").classList.toggle("active", lib);
  $("navSync").setAttribute("aria-selected", String(!lib));
  $("navLibrary").setAttribute("aria-selected", String(lib));
  if (lib) window.dispatchEvent(new CustomEvent("library:show"));
};
$("navSync").onclick = () => showView("sync");
$("navLibrary").onclick = () => showView("library");

// ---- validation --------------------------------------------------------------

const validateConnForm = () => {
  let ok = true;
  setFieldError($("server"), $("serverErr"), "");
  setFieldError($("email"), $("emailErr"), "");
  setFieldError($("mount"), $("mountErr"), "");
  setFieldError($("code"), $("codeErr"), "");

  if (!val("server")) {
    setFieldError($("server"), $("serverErr"), "Enter the sync server URL.");
    ok = false;
  }
  if (!val("email")) {
    setFieldError($("email"), $("emailErr"), "Enter your account email.");
    ok = false;
  }
  if (!val("mount")) {
    setFieldError($("mount"), $("mountErr"), "Choose where to mount the drive.");
    ok = false;
  }
  if (mode === "pair" && !val("code")) {
    setFieldError($("code"), $("codeErr"), "Enter the pairing code from a signed-in device.");
    ok = false;
  }
  return ok;
};

// ---- mount / pair ------------------------------------------------------------

$("mountBtn").onclick = async () => {
  hideToast($("connToast"));
  if (!validateConnForm()) return;
  setStatus(mode === "pair" ? "pairing" : "mounting");
  await withBusy($("mountBtn"), mode === "pair" ? "Pairing…" : "Mounting…", async () => {
    try {
      const a = { ...args(), mountpoint: val("mount") };
      const cmd = mode === "pair" ? "sync_pair" : "sync_start";
      if (mode === "pair") a.code = val("code");
      const result = await invoke(cmd, a);
      toast($("connToast"), result, "ok");
      await refreshStatus();
      loadFiles();
    } catch (e) {
      lastStatus = "unmounted";
      await refreshStatus();
      toast($("connToast"), errText(e), "err");
    }
  });
};

$("unmountBtn").onclick = async () => {
  hideToast($("connToast"));
  await withBusy($("unmountBtn"), "Unmounting…", async () => {
    try {
      const result = await invoke("sync_stop");
      toast($("connToast"), result, "ok");
    } catch (e) {
      toast($("connToast"), errText(e), "err");
    }
    await refreshStatus();
  });
};

// ---- pairing code ------------------------------------------------------------

$("pairCodeBtn").onclick = async () => {
  hideToast($("pairToast"));
  $("codeboxWrap").classList.add("hidden");
  if (!val("server") || !val("email")) {
    toast($("pairToast"), "Enter a server URL and email first.", "err");
    return;
  }
  await withBusy($("pairCodeBtn"), "Generating…", async () => {
    try {
      const code = await invoke("create_pairing", args());
      $("codebox").textContent = code;
      $("codeboxWrap").classList.remove("hidden");
    } catch (e) {
      toast($("pairToast"), errText(e), "err");
    }
  });
};

$("copyCodeBtn").onclick = async () => {
  const code = $("codebox").textContent;
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code);
    toast($("pairToast"), "Copied to clipboard.", "ok");
  } catch {
    toast($("pairToast"), "Couldn't copy — select the code and copy manually.", "err");
  }
};

// ---- file browser + versions -------------------------------------------------

const setFilesState = (html) => {
  const host = $("files");
  host.classList.remove("hidden");
  host.innerHTML = html;
};

const showFilesError = (msg) => {
  $("filesHint").textContent = "";
  setFilesState(
    `<div class="state error"><span class="glyph">⚠️</span>${escHtml(msg)}` +
      `<div class="retry"><button id="filesRetry">Try again</button></div></div>`
  );
  $("filesRetry").onclick = loadFiles;
};

const loadFiles = async () => {
  $("filesHint").textContent = "";
  // skeleton loading state
  setFilesState(`<div class="skeleton"></div><div class="skeleton"></div><div class="skeleton"></div>`);

  let entries;
  try {
    entries = await invoke("browse", args());
  } catch (e) {
    showFilesError(errText(e));
    return;
  }

  if (!entries.length) {
    $("filesHint").textContent = "";
    setFilesState(
      `<div class="state"><span class="glyph">📂</span>No files yet.<br>` +
        `<span class="muted">Save a file into the mounted drive and it'll appear here.</span></div>`
    );
    return;
  }

  const byParent = {};
  for (const e of entries) (byParent[e.parent_node_id || "root"] ||= []).push(e);
  const sortKids = (a, b) =>
    a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "dir" ? -1 : 1;

  const host = $("files");
  host.classList.remove("hidden");
  host.innerHTML = "";
  $("filesHint").textContent = `${entries.length} item${entries.length === 1 ? "" : "s"}`;

  const render = (parentKey, depth) => {
    for (const e of (byParent[parentKey] || []).sort(sortKids)) {
      const row = document.createElement("div");
      row.className = "file" + (e.kind === "dir" ? " dir" : "");
      row.setAttribute("role", "treeitem");
      row.style.paddingLeft = `${0.7 + depth * 1.1}rem`;

      const icon = document.createElement("span");
      icon.setAttribute("aria-hidden", "true");
      icon.textContent = e.kind === "dir" ? "📁" : "📄";
      row.appendChild(icon);

      const name = document.createElement("span");
      name.className = "name";
      name.textContent = e.name;
      row.appendChild(name);

      if (e.locked_by) {
        const lock = document.createElement("span");
        lock.className = "lock";
        lock.title = `Being edited on ${e.locked_by} — you can still open it; saving may conflict.`;
        lock.textContent = `🔒 ${e.locked_by}`;
        row.appendChild(lock);
      }

      if (e.kind === "file") {
        const size = document.createElement("span");
        size.className = "meta";
        size.textContent = fmtSize(e.size);
        row.appendChild(size);

        const verBtn = document.createElement("button");
        verBtn.className = "link";
        verBtn.textContent = "versions";
        verBtn.setAttribute("aria-expanded", "false");
        verBtn.onclick = () => toggleVersions(row, e.node_id, verBtn);
        row.appendChild(verBtn);
      }

      host.appendChild(row);
      if (e.kind === "dir") render(e.node_id, depth + 1);
    }
  };
  render("root", 0);
};

const toggleVersions = async (afterRow, nodeId, verBtn) => {
  const existing = afterRow.nextElementSibling;
  if (existing && existing.classList.contains("versions")) {
    existing.remove();
    verBtn.setAttribute("aria-expanded", "false");
    return;
  }
  verBtn.setAttribute("aria-expanded", "true");

  const panel = document.createElement("div");
  panel.className = "versions";
  panel.innerHTML = `<div class="muted"><span class="spinner"></span>loading versions…</div>`;
  afterRow.after(panel);

  let versions;
  try {
    versions = await invoke("file_versions", { ...args(), nodeId });
  } catch (e) {
    panel.innerHTML = `<div class="err">${escHtml(errText(e))}</div>`;
    return;
  }
  if (!versions.length) {
    panel.innerHTML = `<div class="muted">No earlier versions yet.</div>`;
    return;
  }

  panel.innerHTML = "";
  versions.forEach((v, i) => {
    const r = document.createElement("div");
    r.className = "version";
    const when = document.createElement("span");
    when.className = "when";
    if (i === 0) {
      when.innerHTML = `<span class="current-tag">● current</span> · ${fmtWhen(v.created_at)} · ${fmtSize(v.size)}`;
    } else {
      when.textContent = `${fmtWhen(v.created_at)} (${relWhen(v.created_at)}) · ${fmtSize(v.size)}`;
    }
    r.appendChild(when);

    if (i !== 0) {
      const b = document.createElement("button");
      b.className = "link";
      b.textContent = "restore";
      b.onclick = async () => {
        const ok = window.confirm(
          `Restore this version from ${fmtWhen(v.created_at)}?\n\n` +
            `The current version is kept in history — nothing is lost.`
        );
        if (!ok) return;
        const errSlot = document.createElement("span");
        errSlot.className = "err";
        await withBusy(b, "restoring…", async () => {
          try {
            await invoke("restore_version", { ...args(), nodeId, versionId: v.version_id });
            panel.remove();
            loadFiles();
          } catch (e) {
            errSlot.textContent = ` ${errText(e)}`;
            r.appendChild(errSlot);
          }
        });
      };
      r.appendChild(b);
    }
    panel.appendChild(r);
  });
};

$("refreshFilesBtn").onclick = loadFiles;

// ---- per-OS mount default ----------------------------------------------------

(async () => {
  try {
    const os = await invoke("platform");
    if (os === "windows") {
      $("mount").value = "C:\\Users\\you\\MakerCF";
      $("mountLabel").innerHTML =
        "Mount point — a sync-root <b>folder</b> (e.g. <code>%USERPROFILE%\\MakerCF</code>) · Windows / CfAPI";
    } else if (os === "macos") {
      $("mountLabel").innerHTML =
        "Mount point — a directory (e.g. <code>~/Maker</code>) · macOS / WebDAV";
    } else {
      $("mountLabel").innerHTML =
        "Mount point — a directory (e.g. <code>~/Maker</code>) · Linux / FUSE";
    }
  } catch {
    /* keep default */
  }
})();

// ---- conflict resolution -----------------------------------------------------

let activeConflict = null;
let conflictQueue = [];

const closeConflict = () => {
  activeConflict = null;
  $("overlay").classList.remove("show");
  $("conflictErr").textContent = "";
};

const renderSide = (el, side, label, isIncoming, nodeId) => {
  el.innerHTML = "";
  const who = document.createElement("div");
  who.className = "who";
  who.textContent = label;
  const badge = document.createElement("span");
  badge.className = "badge";
  badge.textContent = isIncoming ? "incoming" : "on server";
  who.appendChild(badge);
  el.appendChild(who);

  if (!side) {
    const meta = document.createElement("div");
    meta.className = "meta";
    meta.textContent = "unavailable";
    el.appendChild(meta);
    return;
  }

  const meta = document.createElement("div");
  meta.className = "meta";
  meta.innerHTML = `edited by <b></b><br>${fmtWhen(side.created_at)} · `;
  meta.querySelector("b").textContent = side.device_name;
  meta.append(fmtSize(side.size));
  el.appendChild(meta);

  const btn = document.createElement("button");
  btn.className = "primary";
  btn.textContent = "Keep this version";
  btn.onclick = async () => {
    $("conflictErr").textContent = "";
    await withBusy(btn, "Keeping…", async () => {
      try {
        await invoke("resolve_conflict_keep", { ...args(), nodeId, versionId: side.version_id });
        afterResolve();
      } catch (e) {
        $("conflictErr").textContent = errText(e);
      }
    });
  };
  el.appendChild(btn);
};

const afterResolve = () => {
  closeConflict();
  loadFiles();
  // surface the next queued conflict, if any
  if (conflictQueue.length) setTimeout(pollConflicts, 300);
};

const showConflict = (c, index, total) => {
  activeConflict = c;
  $("conflictErr").textContent = "";
  $("conflictName").textContent = c.name || c.node_id;
  $("conflictCount").textContent = total > 1 ? `${index + 1} of ${total} conflicts` : "";
  renderSide($("sideCurrent"), c.current, "Version on the server", false, c.node_id);
  renderSide($("sideIncoming"), c.incoming, "Your other edit", true, c.node_id);
  $("overlay").classList.add("show");
  $("conflictClose").focus();
};

$("splitBtn").onclick = async () => {
  if (!activeConflict) return;
  $("conflictErr").textContent = "";
  await withBusy($("splitBtn"), "Saving copy…", async () => {
    try {
      await invoke("resolve_conflict_split", { ...args(), nodeId: activeConflict.node_id });
      afterResolve();
    } catch (e) {
      $("conflictErr").textContent = errText(e);
    }
  });
};

// "decide later" — close without resolving; it'll resurface on the next poll.
$("conflictClose").onclick = closeConflict;
$("overlay").addEventListener("mousedown", (e) => {
  if (e.target === $("overlay")) closeConflict();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && $("overlay").classList.contains("show")) closeConflict();
});

const pollConflicts = async () => {
  if (activeConflict) return; // a prompt is already up
  let list;
  try {
    list = await invoke("conflicts", args());
  } catch {
    return; // not signed in yet, etc.
  }
  conflictQueue = list || [];
  if (conflictQueue.length) showConflict(conflictQueue[0], 0, conflictQueue.length);
};

// ---- boot --------------------------------------------------------------------

// Shared with library.js (kept on window to avoid a bundler/import graph; main.js
// is loaded first, so MakerUI exists before library.js's event handlers fire).
window.MakerUI = { invoke, $, val, toast, withBusy, fmtSize, fmtWhen, relWhen, args };

refreshStatus();
setInterval(refreshStatus, 3000);
setInterval(pollConflicts, 5000);
