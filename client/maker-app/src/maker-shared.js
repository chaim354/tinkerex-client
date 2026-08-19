// Shared helpers for the maker product surfaces (projects / inventory / preview /
// settings). Reads the base helpers from window.MakerUI (set by main.js) and exposes
// a few extras on window.Maker so the per-surface modules can reuse them.
//
// Kept on window (no bundler) — main.js loads first, this loads before the surfaces.

const UI = window.MakerUI;
const { invoke, $ } = UI;

// HTML-escape any value interpolated into innerHTML.
export const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]),
  );

export const safeHref = (u) => (/^https?:\/\//i.test(String(u || "")) ? String(u) : null);

export const showOverlay = (id) => $(id).classList.add("show");
export const closeOverlay = (id) => $(id).classList.remove("show");

// Wire an overlay's close button + backdrop-click + Escape to a close fn.
export function wireOverlay(overlayId, closeBtnId, onClose) {
  const close = () => onClose();
  $(closeBtnId).onclick = close;
  $(overlayId).addEventListener("mousedown", (e) => {
    if (e.target === $(overlayId)) close();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && $(overlayId).classList.contains("show")) close();
  });
}

// A tiny inline toast helper bound to a specific element id.
export const toastTo = (id, text, kind = "info") => UI.toast($(id), text, kind);

// ----------------------------------------------------------------------------
// Shared file picker: lists the synced tree via `browse` and resolves to the
// chosen file's { node_id, name }. Reused by projects/BOM/preview to attach or
// reference a design file by its stable node_id (the file-layer seam).
// ----------------------------------------------------------------------------

let pickerResolve = null;

async function loadPickerList(filter = "") {
  const list = $("mkPickerList");
  $("mkPickerErr").textContent = "";
  if (!UI.signedIn()) {
    list.innerHTML = "";
    $("mkPickerErr").textContent = UI.SIGNED_OUT_HINT;
    return;
  }
  list.innerHTML = `<div class="muted"><span class="spinner"></span>loading files…</div>`;
  let entries;
  try {
    entries = await invoke("browse", UI.args());
  } catch (e) {
    $("mkPickerErr").textContent = String(e);
    list.innerHTML = "";
    return;
  }
  const files = entries
    .filter((e) => e.kind === "file")
    .filter((e) => !filter || e.name.toLowerCase().includes(filter.toLowerCase()))
    .sort((a, b) => a.name.localeCompare(b.name));

  list.innerHTML = "";
  if (!files.length) {
    list.innerHTML = `<div class="muted">No matching files.</div>`;
    return;
  }
  for (const f of files) {
    const row = document.createElement("button");
    row.className = "picker-row";
    row.setAttribute("role", "option");
    row.innerHTML = `<span class="picker-name">📄 ${esc(f.name)}</span>` +
      `<span class="picker-meta">${UI.fmtSize(f.size)}</span>`;
    row.onclick = () => {
      closeOverlay("mkPickerOverlay");
      if (pickerResolve) {
        pickerResolve({ node_id: f.node_id, name: f.name });
        pickerResolve = null;
      }
    };
    list.appendChild(row);
  }
}

/** Open the file picker; resolves to { node_id, name } or null if cancelled. */
export function pickFile(title = "Choose a file") {
  $("mkPickerTitle").textContent = title;
  $("mkPickerFilter").value = "";
  showOverlay("mkPickerOverlay");
  loadPickerList("");
  $("mkPickerFilter").focus();
  return new Promise((resolve) => {
    pickerResolve = resolve;
  });
}

// Picker wiring (filter as you type; cancel resolves null).
const cancelPick = () => {
  closeOverlay("mkPickerOverlay");
  if (pickerResolve) {
    pickerResolve(null);
    pickerResolve = null;
  }
};
$("mkPickerFilter").addEventListener("input", () => loadPickerList($("mkPickerFilter").value.trim()));
wireOverlay("mkPickerOverlay", "mkPickerClose", cancelPick);

// Expose the bundle other surfaces import via window (no shared import graph needed,
// but they DO import this module so load order is guaranteed).
window.Maker = { esc, safeHref, showOverlay, closeOverlay, wireOverlay, toastTo, pickFile };
