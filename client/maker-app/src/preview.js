// Preview surface: a thumbnail gallery. Lists previews via the previews API and lets
// the user enqueue a render for a design file (picked from the synced tree).
//
// Tauri commands:
//   maker_list_previews {…,nodeId?,versionId?,kind?}
//   maker_enqueue_preview {…,nodeId,versionId?,kind?,filename?}

import { esc, safeHref, pickFile } from "/maker-shared.js";

const UI = window.MakerUI;
const { invoke, $ } = UI;
const a = () => UI.args();

// Map node_id → friendly file name from the synced tree (best-effort, for captions).
let nameByNode = {};

async function refreshNames() {
  try {
    const entries = await invoke("browse", a());
    nameByNode = {};
    for (const e of entries) nameByNode[e.node_id] = e.name;
  } catch {
    /* leave names empty */
  }
}

async function loadPreviews() {
  $("prevHint").textContent = "";
  // See library.js#loadCatalog.
  if (!UI.signedIn()) {
    $("prevGrid").innerHTML = `<div class="muted">${esc(UI.SIGNED_OUT_HINT)}</div>`;
    return;
  }
  $("prevGrid").innerHTML = `<div class="skeleton"></div><div class="skeleton"></div>`;
  await refreshNames();
  let previews;
  try {
    previews = await invoke("maker_list_previews", a());
  } catch (e) {
    $("prevGrid").innerHTML = `<div class="err">${esc(String(e))}</div>`;
    return;
  }
  renderPreviews(previews);
}

function statusBadge(status) {
  const cls = { ready: "ok", pending: "busy", failed: "err", processing: "busy" }[status] || "";
  return `<span class="prev-status ${cls}">${esc(status)}</span>`;
}

function renderPreviews(previews) {
  const grid = $("prevGrid");
  grid.innerHTML = "";
  $("prevEmpty").classList.toggle("hidden", previews.length > 0);
  $("prevHint").textContent = previews.length
    ? `${previews.length} preview${previews.length === 1 ? "" : "s"}`
    : "";

  for (const p of previews) {
    const card = document.createElement("div");
    card.className = "prev-card";
    const caption = nameByNode[p.node_id] || p.node_id;
    const href = safeHref(p.url);
    const thumb = href
      ? `<div class="prev-thumb" style="background-image:url('${esc(href)}')"></div>`
      : `<div class="prev-thumb empty">${p.status === "pending" || p.status === "processing"
          ? '<span class="spinner"></span>'
          : "🖼️"}</div>`;
    card.innerHTML = `
      ${thumb}
      <div class="prev-cap" title="${esc(caption)}">${esc(caption)}</div>
      <div class="prev-meta">${statusBadge(p.status)} <span class="muted">${esc(p.kind)}</span></div>
      ${p.error ? `<div class="err prev-error">${esc(p.error)}</div>` : ""}
    `;
    if (href) {
      card.style.cursor = "zoom-in";
      card.onclick = () => window.open(href, "_blank", "noopener");
    } else {
      // let the user re-enqueue a pending/failed one
      card.onclick = () => enqueueFor(p.node_id);
    }
    grid.appendChild(card);
  }
}

async function enqueueFor(nodeId, filename) {
  try {
    await invoke("maker_enqueue_preview", { ...a(), nodeId, kind: "thumbnail", filename });
    UI.toast($("connToast"), "Preview queued — refresh in a moment.", "ok");
    // optimistic refresh after a short delay (the job runs async)
    setTimeout(loadPreviews, 800);
  } catch (e) {
    UI.toast($("connToast"), String(e), "err");
  }
}

$("prevRefresh").onclick = loadPreviews;

$("prevEnqueue").onclick = async () => {
  const picked = await pickFile("Pick a design file to render");
  if (!picked) return;
  await enqueueFor(picked.node_id, picked.name);
};

// ---- lazy load on first show -------------------------------------------------

window.addEventListener("view:show", (e) => {
  if (e.detail === "preview") loadPreviews();
});
