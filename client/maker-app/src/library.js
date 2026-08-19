// Maker Library view. Reads shared helpers from window.MakerUI (set by main.js).
//
// Tauri commands (snake_case fn ↔ camelCase args; Tauri converts automatically):
//   maker_list_items {serverUrl,email,itemType?,tag?,q?}   maker_get_item {…,id}
//   maker_create_item/maker_update_item {…,input}          maker_delete_item {…,id}
//   maker_attach_file {…,itemId,input}   maker_detach_file {…,attachmentId}
//   maker_tag_item {…,itemId,name}       maker_untag {…,taggingId}   maker_list_tags {…}
//   maker_list_collections / maker_create_collection {…,name,description?} /
//   maker_collection_items {…,collectionId} /
//   maker_add_to_collection / maker_remove_from_collection {…,collectionId,itemId}
//   maker_list_tasks {…,openOnly}   maker_create_task/maker_update_task {…,input}
//   maker_delete_task {…,id}
//   browse {serverUrl,email}   (reused as the file-picker source)

const UI = window.MakerUI;
const { invoke, $, toast } = UI;

const TYPE_LABEL = {
  model_3d: "3D model",
  cad: "CAD",
  lightburn: "LightBurn",
  electronics: "Electronics",
  other: "Other",
};
const TYPE_ICON = {
  model_3d: "🧊",
  cad: "📐",
  lightburn: "🔥",
  electronics: "🔌",
  other: "📄",
};

let activeTag = null; // tag-name filter, or null

const libArgs = () => UI.args(); // { serverUrl, email }

// Small HTML-escape for any value interpolated into innerHTML.
const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

// Only allow http(s) URLs as hrefs; blocks javascript:/data:/vbscript: etc.
// Returns the URL when safe, otherwise null (caller renders no link).
function safeHref(u) {
  return /^https?:\/\//i.test(String(u || "")) ? String(u) : null;
}

// Overlays use the existing `.overlay.show` convention (see main.js / styles.css).
const showOverlay = (id) => $(id).classList.add("show");
const closeOverlay = (id) => $(id).classList.remove("show");

// ============================================================================
// Task 4 — catalog browse / search / type+tag filter / favorite
// ============================================================================

async function loadCatalog() {
  // Signed out, libArgs() is null and every command below would fail with a raw Tauri
  // deserialization string. Say what to do instead.
  if (!UI.signedIn()) {
    $("libGrid").innerHTML = `<div class="muted">${esc(UI.SIGNED_OUT_HINT)}</div>`;
    $("libEmpty").classList.add("hidden");
    return;
  }
  const a = libArgs();
  const itemType = $("libType").value || null;
  const q = $("libSearch").value.trim() || null;
  try {
    const items = await invoke("maker_list_items", { ...a, itemType, tag: activeTag, q });
    renderGrid(items);
  } catch (e) {
    toast($("connToast"), String(e), "err");
  }
}

function renderGrid(items) {
  const grid = $("libGrid");
  grid.innerHTML = "";
  $("libEmpty").classList.toggle("hidden", items.length > 0);
  for (const it of items) {
    const card = document.createElement("div");
    card.className = "lib-card";
    card.onclick = () => openItem(it.id);
    // Previews seam: when the Previews pillar lands, set the .lib-thumb
    // background-image from it.preview_url — no other UI change needed.
    card.innerHTML = `
      <div class="lib-thumb">${TYPE_ICON[it.item_type] || "📄"}</div>
      <div class="lib-title"></div>
      <span class="badge"></span>
      <span class="fav ${it.favorited ? "on" : ""}" title="Favorite">★</span>`;
    card.querySelector(".lib-title").textContent = it.title;
    card.querySelector(".badge").textContent = TYPE_LABEL[it.item_type] || it.item_type;
    const fav = card.querySelector(".fav");
    fav.onclick = async (ev) => {
      ev.stopPropagation();
      try {
        await invoke("maker_update_item", { ...libArgs(), id: it.id, input: { favorited: !it.favorited } });
        loadCatalog();
      } catch (e) {
        toast($("connToast"), String(e), "err");
      }
    };
    grid.appendChild(card);
  }
}

async function loadTagBar() {
  const bar = $("libTags");
  bar.innerHTML = "";
  let tags = [];
  try {
    tags = await invoke("maker_list_tags", libArgs());
  } catch {
    return; // empty bar
  }
  for (const t of tags) {
    const chip = document.createElement("button");
    chip.className = "chip" + (activeTag === t.name ? " on" : "");
    chip.textContent = t.name;
    chip.onclick = () => {
      activeTag = activeTag === t.name ? null : t.name;
      loadTagBar();
      loadCatalog();
    };
    bar.appendChild(chip);
  }
}

// Debounced search + type filter.
let searchTimer;
$("libSearch").addEventListener("input", () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(loadCatalog, 200);
});
$("libType").addEventListener("change", loadCatalog);

// ============================================================================
// Task 5 — item detail, create/edit form, attach/detach via the file picker
// ============================================================================

// Minimal, safe markdown: escape, then ### h4, `code`, **bold**, *italic*, line breaks.
// (Avoids a dependency; instructions are short. A real renderer is a later polish.)
function mdLite(src) {
  return esc(src || "")
    .replace(/^### (.*)$/gm, "<h4>$1</h4>")
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\*([^*]+)\*/g, "<em>$1</em>")
    .replace(/\n/g, "<br>");
}

async function openItem(id) {
  const a = libArgs();
  let it;
  try {
    it = await invoke("maker_get_item", { ...a, id });
  } catch (e) {
    return toast($("connToast"), String(e), "err");
  }
  $("itemTitle").textContent = it.title;
  const atts = (it.file_attachments || [])
    .map(
      (f) => `
    <div class="attach-row">
      <span>${esc(f.role)} · ${esc(f.label || f.node_id)}</span>
      <button data-att="${f.id}" class="detach">Detach</button>
    </div>`,
    )
    .join("");
  // Prefer the taggings (carry the join-row id → removable); fall back to plain tag names.
  const taggings = it.taggings && it.taggings.length ? it.taggings : (it.tags || []).map((name) => ({ id: null, name }));
  const tagChips =
    taggings
      .map(
        (t) =>
          `<span class="chip on">${esc(t.name)}${t.id != null ? ` <button data-untag="${t.id}" class="untag" title="Remove tag" aria-label="Remove tag">×</button>` : ""}</span>`,
      )
      .join("") || '<span class="muted">none</span>';
  $("itemBody").innerHTML = `
    <p class="muted">${esc(TYPE_LABEL[it.item_type] || it.item_type)} · ${esc(it.origin)}${
      it.source_url
        ? safeHref(it.source_url)
          ? ` · <a href="${esc(safeHref(it.source_url))}" target="_blank" rel="noreferrer noopener">source</a>`
          : ` · ${esc(it.source_url)}`
        : ""
    }</p>
    ${it.summary ? `<p>${esc(it.summary)}</p>` : ""}
    <div class="instructions">${mdLite(it.instructions)}</div>
    <h4>Files</h4>${atts || '<p class="muted">No files attached.</p>'}
    <div class="row" style="margin-top:.4rem">
      <button id="itemAttach">Attach a file</button>
      <button id="itemEdit">Edit</button>
      <button id="itemDelete" class="danger">Delete</button>
    </div>
    <h4>Tags</h4>
    <div class="row" style="margin-top:0"><input id="itemTagInput" placeholder="add a tag…"/><button id="itemTagAdd">Add</button></div>
    <div class="lib-tagbar">${tagChips}</div>`;
  // wire actions
  $("itemBody").querySelectorAll(".detach").forEach(
    (b) =>
      (b.onclick = async () => {
        try {
          await invoke("maker_detach_file", { ...libArgs(), attachmentId: Number(b.dataset.att) });
          openItem(id);
        } catch (e) {
          toast($("connToast"), String(e), "err");
        }
      }),
  );
  $("itemBody").querySelectorAll(".untag").forEach(
    (b) =>
      (b.onclick = async (ev) => {
        ev.stopPropagation();
        try {
          await invoke("maker_untag", { ...libArgs(), taggingId: Number(b.dataset.untag) });
          openItem(id);
          loadTagBar();
        } catch (e) {
          toast($("connToast"), String(e), "err");
        }
      }),
  );
  $("itemAttach").onclick = () => openPicker(id);
  $("itemEdit").onclick = () => openItemForm(it);
  $("itemDelete").onclick = async () => {
    try {
      await invoke("maker_delete_item", { ...libArgs(), id });
      closeOverlay("itemOverlay");
      loadCatalog();
    } catch (e) {
      toast($("connToast"), String(e), "err");
    }
  };
  $("itemTagAdd").onclick = async () => {
    const name = $("itemTagInput").value.trim();
    if (!name) return;
    try {
      await invoke("maker_tag_item", { ...libArgs(), itemId: id, name });
      openItem(id);
      loadTagBar();
    } catch (e) {
      toast($("connToast"), String(e), "err");
    }
  };
  showOverlay("itemOverlay");
}

function openItemForm(existing) {
  const a = libArgs();
  const it = existing || { item_type: "other", origin: "downloaded" };
  $("itemTitle").textContent = existing ? "Edit item" : "New item";
  const opt = (v, cur, label) => `<option value="${v}" ${v === cur ? "selected" : ""}>${label}</option>`;
  $("itemBody").innerHTML = `
    <label>Title</label><input id="fTitle" value="${esc(it.title || "")}"/>
    <label>Type</label><select id="fType">
      ${opt("model_3d", it.item_type, "3D model")}${opt("cad", it.item_type, "CAD")}
      ${opt("lightburn", it.item_type, "LightBurn")}${opt("electronics", it.item_type, "Electronics")}
      ${opt("other", it.item_type, "Other")}</select>
    <label>Origin</label><select id="fOrigin">
      ${opt("self_made", it.origin, "Self-made")}${opt("downloaded", it.origin, "Downloaded")}
      ${opt("modified", it.origin, "Modified")}</select>
    <label>Summary</label><input id="fSummary" value="${esc(it.summary || "")}"/>
    <label>Source URL</label><input id="fSource" value="${esc(it.source_url || "")}"/>
    <label>Instructions (markdown)</label><textarea id="fInstr" rows="6">${esc(it.instructions || "")}</textarea>
    <div class="row" style="margin-top:.5rem"><button id="fSave" class="primary">Save</button></div>`;
  $("fSave").onclick = async () => {
    const input = {
      title: $("fTitle").value.trim(),
      item_type: $("fType").value,
      origin: $("fOrigin").value,
      summary: $("fSummary").value.trim() || null,
      source_url: $("fSource").value.trim() || null,
      instructions: $("fInstr").value || null,
    };
    if (!input.title) return toast($("connToast"), "Title is required", "err");
    try {
      const saved = existing
        ? await invoke("maker_update_item", { ...a, id: existing.id, input })
        : await invoke("maker_create_item", { ...a, input });
      loadCatalog();
      openItem(saved.id);
    } catch (e) {
      toast($("connToast"), String(e), "err");
    }
  };
  showOverlay("itemOverlay");
}
$("libNewItem").onclick = () => openItemForm(null);

async function openPicker(itemId) {
  showOverlay("pickerOverlay");
  const list = $("pickerList");
  list.innerHTML = "Loading…";
  let entries = [];
  try {
    entries = await invoke("browse", libArgs());
  } catch (e) {
    list.innerHTML = "";
    return toast($("pickerErr"), String(e), "err");
  }
  const files = entries.filter((e) => e.kind === "file");
  const render = (filter) => {
    list.innerHTML = "";
    if (!files.length) {
      list.innerHTML = '<p class="muted">No synced files yet.</p>';
      return;
    }
    for (const f of files.filter((f) => !filter || f.name.toLowerCase().includes(filter))) {
      const row = document.createElement("div");
      row.className = "picker-row";
      row.innerHTML = `<span></span><span class="muted">${UI.fmtSize(f.size)}</span>`;
      row.querySelector("span").textContent = f.name;
      row.onclick = async () => {
        const role = guessRole(f.name);
        try {
          await invoke("maker_attach_file", { ...libArgs(), itemId, input: { node_id: f.node_id, role, label: f.name } });
          closeOverlay("pickerOverlay");
          openItem(itemId);
        } catch (e) {
          toast($("pickerErr"), String(e), "err");
        }
      };
      list.appendChild(row);
    }
  };
  $("pickerFilter").oninput = () => render($("pickerFilter").value.trim().toLowerCase());
  render("");
}

// Roles must match the server's Maker::FileAttachment::ROLES =
//   %w[model instructions image source_archive gcode other]
function guessRole(name) {
  const n = name.toLowerCase();
  if (/\.(stl|3mf|obj|step|stp|iges|igs|f3d|fcstd|sldprt)$/.test(n)) return "model";
  if (/\.(png|jpg|jpeg|webp|gif)$/.test(n)) return "image";
  if (/\.(md|txt|pdf)$/.test(n)) return "instructions";
  if (/\.(gcode|gco|nc)$/.test(n)) return "gcode";
  if (/\.(zip|7z|rar)$/.test(n)) return "source_archive";
  return "other";
}

$("itemClose").onclick = () => closeOverlay("itemOverlay");
$("pickerClose").onclick = () => closeOverlay("pickerOverlay");
// Click the backdrop to dismiss (matches the conflict overlay behavior).
$("itemOverlay").addEventListener("mousedown", (e) => {
  if (e.target === $("itemOverlay")) closeOverlay("itemOverlay");
});
$("pickerOverlay").addEventListener("mousedown", (e) => {
  if (e.target === $("pickerOverlay")) closeOverlay("pickerOverlay");
});

// ============================================================================
// Task 6 — collections
// ============================================================================

async function loadCollections() {
  const wrap = $("collList");
  wrap.innerHTML = "";
  let cols = [];
  try {
    cols = await invoke("maker_list_collections", libArgs());
  } catch {
    return;
  }
  for (const c of cols) {
    const el = document.createElement("div");
    el.className = "lib-coll";
    el.textContent = `${c.name} (${c.item_count})`;
    el.onclick = () => openCollection(c);
    wrap.appendChild(el);
  }
}
$("collCreate").onclick = async () => {
  const name = $("collName").value.trim();
  if (!name) return;
  try {
    await invoke("maker_create_collection", { ...libArgs(), name, description: null });
    $("collName").value = "";
    loadCollections();
  } catch (e) {
    toast($("connToast"), String(e), "err");
  }
};

async function openCollection(c) {
  $("itemTitle").textContent = c.name;
  let members = [];
  try {
    members = await invoke("maker_collection_items", { ...libArgs(), collectionId: c.id });
  } catch {
    /* empty */
  }
  const rows = members
    .map(
      (m) => `
    <div class="attach-row"><span>${esc(m.title)} <span class="badge">${esc(TYPE_LABEL[m.item_type] || m.item_type)}</span></span>
    <button data-rm="${m.id}" class="rmcoll">Remove</button></div>`,
    )
    .join("");
  $("itemBody").innerHTML = `${rows || '<p class="muted">Empty collection.</p>'}
    <div class="row" style="margin-top:.5rem"><button id="collAddItem">Add an item…</button></div>`;
  $("itemBody").querySelectorAll(".rmcoll").forEach(
    (b) =>
      (b.onclick = async () => {
        try {
          await invoke("maker_remove_from_collection", { ...libArgs(), collectionId: c.id, itemId: Number(b.dataset.rm) });
          openCollection(c);
          loadCollections();
        } catch (e) {
          toast($("connToast"), String(e), "err");
        }
      }),
  );
  $("collAddItem").onclick = async () => {
    let all = [];
    try {
      all = await invoke("maker_list_items", { ...libArgs(), itemType: null, tag: null, q: null });
    } catch (e) {
      return toast($("connToast"), String(e), "err");
    }
    $("itemBody").innerHTML =
      all.map((it) => `<div class="picker-row" data-add="${it.id}">${esc(it.title)}</div>`).join("") ||
      '<p class="muted">No items to add.</p>';
    $("itemBody").querySelectorAll("[data-add]").forEach(
      (r) =>
        (r.onclick = async () => {
          try {
            await invoke("maker_add_to_collection", { ...libArgs(), collectionId: c.id, itemId: Number(r.dataset.add) });
            openCollection(c);
            loadCollections();
          } catch (e) {
            toast($("connToast"), String(e), "err");
          }
        }),
    );
  };
  showOverlay("itemOverlay");
}

// ============================================================================
// Task 7 — tasks board (todo / doing / done)
// ============================================================================

const COLS = [
  ["todo", "To do"],
  ["doing", "Doing"],
  ["done", "Done"],
];

async function loadTasks() {
  const board = $("taskBoard");
  board.innerHTML = "";
  let tasks = [];
  try {
    tasks = await invoke("maker_list_tasks", { ...libArgs(), openOnly: false });
  } catch {
    return;
  }
  for (const [status, label] of COLS) {
    const col = document.createElement("div");
    col.className = "lib-col";
    col.innerHTML = `<h4>${label}</h4>`;
    for (const t of tasks.filter((x) => x.status === status)) {
      const el = document.createElement("div");
      el.className = "task";
      const next = status === "todo" ? "doing" : status === "doing" ? "done" : "todo";
      el.innerHTML = `<span></span><span class="task-acts">
        <button data-mv>${status === "done" ? "↺" : "→"}</button>
        <button data-del>✕</button></span>`;
      el.querySelector("span").textContent = t.title;
      el.querySelector("[data-mv]").onclick = async () => {
        try {
          await invoke("maker_update_task", { ...libArgs(), id: t.id, input: { status: next } });
          loadTasks();
        } catch (e) {
          toast($("connToast"), String(e), "err");
        }
      };
      el.querySelector("[data-del]").onclick = async () => {
        try {
          await invoke("maker_delete_task", { ...libArgs(), id: t.id });
          loadTasks();
        } catch (e) {
          toast($("connToast"), String(e), "err");
        }
      };
      col.appendChild(el);
    }
    board.appendChild(col);
  }
}
$("taskCreate").onclick = async () => {
  const title = $("taskTitle").value.trim();
  if (!title) return;
  try {
    await invoke("maker_create_task", { ...libArgs(), input: { title } });
    $("taskTitle").value = "";
    loadTasks();
  } catch (e) {
    toast($("connToast"), String(e), "err");
  }
};

// ============================================================================
// Load everything when the Library view is shown.
// ============================================================================

window.addEventListener("library:show", () => {
  loadTagBar();
  loadCatalog();
  loadCollections();
  loadTasks();
});

export { loadCatalog, loadTagBar, loadCollections, loadTasks, libArgs, TYPE_LABEL };
