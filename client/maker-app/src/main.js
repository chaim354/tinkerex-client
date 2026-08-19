// Maker client — frontend logic.
//
// Vanilla ES module. All backend calls go through the Tauri `invoke` bridge.
// The command names + argument casing here MUST match src-tauri/src/lib.rs:
//   sync_login / sync_stop / sync_status / platform /
//   browse / file_versions / restore_version / conflicts /
//   resolve_conflict_keep / resolve_conflict_split
// args use camelCase: serverUrl, email, password, mountpoint, nodeId, versionId.

// Root-relative like the other modules' "/maker-shared.js" — tauri.conf.json serves src/ as the
// frontend root. It does NOT disturb the load order window.MakerUI depends on: a dependency is
// evaluated before its importer, and the <script type="module"> tags still run in document order,
// so main.js is still fully evaluated before library.js and the rest.
import { accountArgs, accountAfter, fetchConflicts, signedIn, SIGNED_OUT_HINT } from "/session.js";

const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const val = (id) => $(id).value.trim();

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

// The account signed in on this device, snapshotted from the form at sign-in. browse /
// file_versions / the conflict commands read THIS, and so does every maker_* command in the
// other surface modules (via window.MakerUI.args) — never the live form fields, or editing the
// sign-in form afterwards would silently retarget them at a different account.
//
// IT OUTLIVES THE MOUNT. It is set on a successful sign-in and replaced by the next one; nothing
// else touches it, and unmounting in particular does NOT clear it. It used to, which restored
// exactly the hazard the paragraph above describes — see src/session.js for the whole argument.
// Null before the first sign-in of this process, and args() then answers null so the commands
// refuse rather than aiming at index.html's dev defaults.
let signedInAccount = null;
const args = () =>
  accountArgs(signedInAccount, { serverUrl: val("server"), email: val("email") });

// ---- status pill -------------------------------------------------------------

let lastStatus = "unmounted";
const setStatus = (text) => {
  lastStatus = text;
  const pill = $("statusPill");
  const label = $("statusText");
  if (text.startsWith("mounted")) {
    pill.className = "pill on";
    label.textContent = text; // "mounted at <path>"
  } else if (text === "mounting") {
    pill.className = "pill busy";
    label.textContent = "signing in…";
  } else {
    pill.className = "pill";
    label.textContent = "unmounted";
  }
  $("unmountBtn").disabled = !text.startsWith("mounted");
};

const refreshStatus = async () => {
  // don't clobber a transient "mounting" the click handler set
  if (lastStatus === "mounting") return;
  try {
    setStatus(await invoke("sync_status"));
  } catch {
    setStatus("unmounted");
  }
};

// ---- top-level view nav (data-driven) ----------------------------------------
//
// Each nav button carries data-view; the matching container is `view<Capitalized>`.
// Switching a view dispatches `view:show` (detail = the view name) so each surface's
// module can lazily (re)load. The legacy `library:show` event is still fired for
// library.js back-compat.

const VIEWS = ["sync", "library", "projects", "inventory", "preview", "settings"];
const cap = (s) => s.charAt(0).toUpperCase() + s.slice(1);

const showView = (which) => {
  for (const v of VIEWS) {
    const isActive = v === which;
    const container = $("view" + cap(v));
    const nav = $("nav" + cap(v));
    if (container) container.classList.toggle("hidden", !isActive);
    if (nav) {
      nav.classList.toggle("active", isActive);
      nav.setAttribute("aria-selected", String(isActive));
    }
  }
  if (which === "library") window.dispatchEvent(new CustomEvent("library:show"));
  window.dispatchEvent(new CustomEvent("view:show", { detail: which }));
};

document.querySelectorAll(".viewnav button[data-view]").forEach((btn) => {
  btn.onclick = () => showView(btn.dataset.view);
});

// ---- validation --------------------------------------------------------------

const validateConnForm = () => {
  let ok = true;
  setFieldError($("server"), $("serverErr"), "");
  setFieldError($("email"), $("emailErr"), "");
  setFieldError($("password"), $("passwordErr"), "");
  setFieldError($("mount"), $("mountErr"), "");

  if (!val("server")) {
    setFieldError($("server"), $("serverErr"), "Enter the sync server URL.");
    ok = false;
  }
  if (!val("email")) {
    setFieldError($("email"), $("emailErr"), "Enter your account email.");
    ok = false;
  }
  // Don't trim the password — it may legitimately contain whitespace.
  if (!$("password").value) {
    setFieldError($("password"), $("passwordErr"), "Enter your account password.");
    ok = false;
  }
  if (!val("mount")) {
    setFieldError($("mount"), $("mountErr"), "Choose where to mount the drive.");
    ok = false;
  }
  return ok;
};

// ---- sign in (device login) --------------------------------------------------

$("mountBtn").onclick = async () => {
  hideToast($("connToast"));
  if (!validateConnForm()) return;
  setStatus("mounting");
  await withBusy($("mountBtn"), "Signing in…", async () => {
    try {
      // sync_login(server_url, email, password, mountpoint) — Tauri maps camelCase→snake_case.
      // The device enrolls by logging in; the server returns a per-device token (saved
      // locally), then we mount + start the sync loop. Reused on later launches.
      const result = await invoke("sync_login", {
        serverUrl: val("server"),
        email: val("email"),
        password: $("password").value,
        mountpoint: val("mount"),
      });
      // Pin the commands to the account we just signed in as, so later form edits can't
      // retarget them. This is the ONLY thing that changes the account — a re-sign-in with
      // different credentials is the explicit account switch.
      signedInAccount = accountAfter(signedInAccount, {
        type: "signed-in",
        serverUrl: val("server"),
        email: val("email"),
      });
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
      // Deliberately a NO-OP on the account: unmounting stops the drive, it does not sign the
      // device out, and the whole maker catalog goes on working without a mount. Clearing it
      // here is what sent all 64 catalog calls at the sign-in form's dev defaults.
      signedInAccount = accountAfter(signedInAccount, { type: "unmounted" });
      toast($("connToast"), result, "ok");
    } catch (e) {
      toast($("connToast"), errText(e), "err");
    }
    await refreshStatus();
  });
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
  if (!signedIn(signedInAccount)) {
    setFilesState(
      `<div class="state"><span class="glyph">🔒</span>Sign in above to browse the synced drive.</div>`
    );
    return;
  }
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
  // Runs on a 5 s interval from boot, with no user action — so it is the one caller that must
  // not ask at all when signed out. fetchConflicts issues no invoke in that case and answers
  // null; null also covers a transient bridge error, where the queue is left as it was.
  const list = await fetchConflicts({ invoke, account: signedInAccount });
  if (!list) return;
  conflictQueue = list;
  if (conflictQueue.length) showConflict(conflictQueue[0], 0, conflictQueue.length);
};

// ---- boot --------------------------------------------------------------------

// Shared with library.js (kept on window to avoid a bundler/import graph; main.js
// is loaded first, so MakerUI exists before library.js's event handlers fire).
// `signedIn()` is how the other surfaces render an explicit signed-out state instead of showing
// the raw Tauri deserialization error a null `args()` produces.
window.MakerUI = {
  invoke, $, val, toast, withBusy, fmtSize, fmtWhen, relWhen, args,
  signedIn: () => signedIn(signedInAccount),
  SIGNED_OUT_HINT,
};

refreshStatus();
setInterval(refreshStatus, 3000);
setInterval(pollConflicts, 5000);
