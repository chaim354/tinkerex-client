// Projects surface: list/create/edit projects, build-log entries, attached files,
// and per-project BOMs (revisions, lines linking inventory item | design-file
// node_id | free-text, with the rollup view).
//
// Tauri commands (camelCase args; Tauri maps to snake_case fns):
//   maker_list_projects {…,status?}  maker_get_project {…,id}
//   maker_create_project/maker_update_project {…,input}  maker_delete_project {…,id}
//   maker_add_build_log {…,projectId,input}  maker_delete_build_log {…,entryId}
//   maker_attach_file_to_project {…,projectId,input}
//   maker_list_boms {…,projectId}  maker_create_bom {…,projectId,input}
//   maker_delete_bom {…,id}  maker_bom_rollup {…,id}
//   maker_create_bom_line {…,bomId,input}  maker_update_bom_line {…,lineId,input}
//   maker_delete_bom_line {…,lineId}
//   maker_list_inventory {…} (to populate the line's inventory dropdown)

import { esc, showOverlay, closeOverlay, wireOverlay, pickFile } from "/maker-shared.js";

const UI = window.MakerUI;
const { invoke, $ } = UI;
const a = () => UI.args();

const STATUS_LABEL = {
  planning: "Planning", active: "Active", paused: "Paused", done: "Done", archived: "Archived",
};
const UNITS = ["each", "g", "kg", "m", "mm", "cm", "ml", "l", "pack", "roll", "sheet", "other"];

// ---- list --------------------------------------------------------------------

async function loadProjects() {
  // See library.js#loadCatalog — signed out, a() is null and the command fails with a raw
  // Tauri deserialization string.
  if (!UI.signedIn()) {
    $("projList").innerHTML = `<div class="muted">${esc(UI.SIGNED_OUT_HINT)}</div>`;
    $("projEmpty").classList.add("hidden");
    return;
  }
  const status = $("projStatus").value || null;
  let projects;
  try {
    projects = await invoke("maker_list_projects", { ...a(), status });
  } catch (e) {
    $("projList").innerHTML = `<div class="err">${esc(String(e))}</div>`;
    return;
  }
  renderProjects(projects);
}

function renderProjects(projects) {
  const host = $("projList");
  host.innerHTML = "";
  $("projEmpty").classList.toggle("hidden", projects.length > 0);
  for (const p of projects) {
    const row = document.createElement("div");
    row.className = "proj-row";
    row.onclick = () => openProject(p.id);
    row.innerHTML =
      `<span class="proj-name">${esc(p.name)}</span>` +
      `<span class="badge status-${esc(p.status)}">${esc(STATUS_LABEL[p.status] || p.status)}</span>` +
      `<span class="proj-meta muted">${p.updated_at ? UI.relWhen(p.updated_at) : ""}</span>`;
    host.appendChild(row);
  }
}

// ---- create ------------------------------------------------------------------

$("projNew").onclick = async () => {
  const name = window.prompt("New project name");
  if (!name || !name.trim()) return;
  // slug: kebab from name (server requires presence + uniqueness per user).
  const slug = name.trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  try {
    const p = await invoke("maker_create_project", {
      ...a(),
      input: { name: name.trim(), slug, status: "planning" },
    });
    await loadProjects();
    openProject(p.id);
  } catch (e) {
    UI.toast($("connToast"), String(e), "err");
  }
};

$("projStatus").addEventListener("change", loadProjects);

// ---- detail overlay ----------------------------------------------------------

let currentProject = null;
const closeProj = () => {
  currentProject = null;
  closeOverlay("projOverlay");
  $("projErr").textContent = "";
};
wireOverlay("projOverlay", "projClose", closeProj);

async function openProject(id) {
  $("projErr").textContent = "";
  $("projBody").innerHTML = `<div class="muted"><span class="spinner"></span>loading…</div>`;
  showOverlay("projOverlay");
  try {
    currentProject = await invoke("maker_get_project", { ...a(), id });
  } catch (e) {
    $("projErr").textContent = String(e);
    return;
  }
  renderProjectDetail(currentProject);
}

function renderProjectDetail(p) {
  $("projTitle").textContent = p.name;
  const statusOpts = Object.entries(STATUS_LABEL)
    .map(([v, l]) => `<option value="${v}"${v === p.status ? " selected" : ""}>${l}</option>`)
    .join("");

  $("projBody").innerHTML = `
    <div class="form-grid">
      <label>Name<input id="pdName" value="${esc(p.name)}" /></label>
      <label>Status<select id="pdStatus">${statusOpts}</select></label>
      <label class="full">Description<textarea id="pdDesc" rows="3">${esc(p.description || "")}</textarea></label>
    </div>
    <div class="row">
      <button id="pdSave" class="primary">Save</button>
      <button id="pdDelete" class="danger">Delete project</button>
    </div>

    <h4>Build log</h4>
    <div class="row" style="margin-top:0">
      <input id="pdLogBody" placeholder="Log a note, milestone…" />
      <select id="pdLogType">
        <option value="note">Note</option>
        <option value="milestone">Milestone</option>
        <option value="issue">Issue</option>
      </select>
      <button id="pdLogAdd">Add</button>
    </div>
    <div id="pdLog" class="log-list"></div>

    <h4>Attached files</h4>
    <div class="row" style="margin-top:0"><button id="pdAttach">Attach a file…</button></div>
    <div id="pdFiles" class="chip-list"></div>

    <h4>Bill of materials</h4>
    <div class="row" style="margin-top:0">
      <select id="pdBomSel" aria-label="BOM revision"></select>
      <button id="pdBomNew">New revision</button>
    </div>
    <div id="pdBom" class="bom-wrap"></div>
  `;

  $("pdSave").onclick = saveProject;
  $("pdDelete").onclick = deleteProject;
  $("pdLogAdd").onclick = addLogEntry;
  $("pdAttach").onclick = attachToProject;
  $("pdBomNew").onclick = createBom;
  $("pdBomSel").onchange = () => loadRollup(Number($("pdBomSel").value));

  renderLog(p.build_log_entries || []);
  renderAttachments(p.file_attachments || []);
  loadBoms();
}

async function saveProject() {
  const input = {
    name: $("pdName").value.trim(),
    status: $("pdStatus").value,
    description: $("pdDesc").value,
  };
  await UI.withBusy($("pdSave"), "Saving…", async () => {
    try {
      currentProject = await invoke("maker_update_project", { ...a(), id: currentProject.id, input });
      $("projTitle").textContent = currentProject.name;
      loadProjects();
    } catch (e) {
      $("projErr").textContent = String(e);
    }
  });
}

async function deleteProject() {
  if (!window.confirm(`Delete project “${currentProject.name}”? This can't be undone.`)) return;
  try {
    await invoke("maker_delete_project", { ...a(), id: currentProject.id });
    closeProj();
    loadProjects();
  } catch (e) {
    $("projErr").textContent = String(e);
  }
}

// ---- build log ---------------------------------------------------------------

function renderLog(entries) {
  const host = $("pdLog");
  host.innerHTML = "";
  if (!entries.length) {
    host.innerHTML = `<div class="muted">No log entries yet.</div>`;
    return;
  }
  for (const e of entries) {
    const row = document.createElement("div");
    row.className = "log-entry";
    row.innerHTML =
      `<span class="badge">${esc(e.entry_type || "note")}</span>` +
      `<span class="log-body">${esc(e.body)}</span>` +
      `<span class="log-when muted">${e.logged_at ? UI.relWhen(e.logged_at) : ""}</span>`;
    const del = document.createElement("button");
    del.className = "link";
    del.textContent = "delete";
    del.onclick = async () => {
      try {
        await invoke("maker_delete_build_log", { ...a(), entryId: e.id });
        openProject(currentProject.id);
      } catch (err) {
        $("projErr").textContent = String(err);
      }
    };
    row.appendChild(del);
    host.appendChild(row);
  }
}

async function addLogEntry() {
  const body = $("pdLogBody").value.trim();
  if (!body) return;
  try {
    await invoke("maker_add_build_log", {
      ...a(),
      projectId: currentProject.id,
      input: { body, entry_type: $("pdLogType").value },
    });
    $("pdLogBody").value = "";
    openProject(currentProject.id);
  } catch (e) {
    $("projErr").textContent = String(e);
  }
}

// ---- attachments -------------------------------------------------------------

function renderAttachments(atts) {
  const host = $("pdFiles");
  host.innerHTML = "";
  if (!atts.length) {
    host.innerHTML = `<div class="muted">No files attached.</div>`;
    return;
  }
  for (const att of atts) {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.textContent = `📄 ${att.label || att.node_id} (${att.role})`;
    host.appendChild(chip);
  }
}

async function attachToProject() {
  const picked = await pickFile("Attach a file to this project");
  if (!picked) return;
  try {
    await invoke("maker_attach_file_to_project", {
      ...a(),
      projectId: currentProject.id,
      input: { node_id: picked.node_id, role: "reference", label: picked.name },
    });
    openProject(currentProject.id);
  } catch (e) {
    $("projErr").textContent = String(e);
  }
}

// ---- BOM ---------------------------------------------------------------------

let currentBomId = null;

async function loadBoms() {
  let boms;
  try {
    boms = await invoke("maker_list_boms", { ...a(), projectId: currentProject.id });
  } catch (e) {
    $("pdBom").innerHTML = `<div class="err">${esc(String(e))}</div>`;
    return;
  }
  const sel = $("pdBomSel");
  sel.innerHTML = boms
    .map((b) => `<option value="${b.id}">${esc(b.name || "BOM")} · rev ${b.revision}</option>`)
    .join("");
  if (!boms.length) {
    sel.innerHTML = `<option value="">No BOM yet</option>`;
    $("pdBom").innerHTML = `<div class="muted">Create a revision to start a parts list.</div>`;
    currentBomId = null;
    return;
  }
  currentBomId = boms[boms.length - 1].id; // newest revision
  sel.value = String(currentBomId);
  loadRollup(currentBomId);
}

async function createBom() {
  try {
    const bom = await invoke("maker_create_bom", {
      ...a(),
      projectId: currentProject.id,
      input: { name: "BOM" },
    });
    currentBomId = bom.id;
    await loadBoms();
  } catch (e) {
    $("projErr").textContent = String(e);
  }
}

async function loadRollup(bomId) {
  if (!bomId) return;
  currentBomId = bomId;
  $("pdBom").innerHTML = `<div class="muted"><span class="spinner"></span>loading BOM…</div>`;
  let rollup;
  try {
    rollup = await invoke("maker_bom_rollup", { ...a(), id: bomId });
  } catch (e) {
    $("pdBom").innerHTML = `<div class="err">${esc(String(e))}</div>`;
    return;
  }
  renderRollup(rollup);
}

function targetLabel(line) {
  const t = line.target || {};
  switch (line.target_kind) {
    case "inventory_item":
      return t.inventory_item
        ? `📦 ${esc(t.inventory_item.name)} (have ${t.inventory_item.quantity_cached})`
        : "📦 (inventory item)";
    case "design_file":
      return `📄 ${esc(t.node_id || "")}`;
    case "free_text":
      return `✏️ ${esc(t.part_name || "")}${t.supplier_name ? " — " + esc(t.supplier_name) : ""}`;
    default:
      return "(unresolved)";
  }
}

function renderRollup(rollup) {
  const host = $("pdBom");
  const rows = rollup.lines
    .map(
      (l) => `
      <tr>
        <td>${esc(l.reference || "")}</td>
        <td>${targetLabel(l)}</td>
        <td class="num">${esc(l.quantity)} ${esc(l.unit)}</td>
        <td>${esc(l.notes || "")}</td>
        <td><button class="link bom-del" data-id="${l.id}">remove</button></td>
      </tr>`,
    )
    .join("");
  host.innerHTML = `
    <table class="bom-table">
      <thead><tr><th>Ref</th><th>Part</th><th class="num">Qty</th><th>Notes</th><th></th></tr></thead>
      <tbody>${rows || `<tr><td colspan="5" class="muted">No lines yet.</td></tr>`}</tbody>
    </table>
    <div class="bom-add">
      <input id="blRef" placeholder="Ref (R1)" class="bl-ref" />
      <select id="blKind">
        <option value="inventory_item">Inventory item</option>
        <option value="design_file">Design file</option>
        <option value="free_text">Free text</option>
      </select>
      <span id="blTargetSlot"></span>
      <input id="blQty" type="number" min="0.001" step="any" value="1" class="bl-qty" />
      <select id="blUnit">${UNITS.map((u) => `<option>${u}</option>`).join("")}</select>
      <button id="blAdd" class="primary">Add line</button>
    </div>
  `;
  host.querySelectorAll(".bom-del").forEach((b) => {
    b.onclick = async () => {
      try {
        await invoke("maker_delete_bom_line", { ...a(), lineId: Number(b.dataset.id) });
        loadRollup(currentBomId);
      } catch (e) {
        $("projErr").textContent = String(e);
      }
    };
  });

  $("blKind").onchange = renderTargetField;
  $("blAdd").onclick = addBomLine;
  renderTargetField();
}

let pickedNodeId = null;

function renderTargetField() {
  const kind = $("blKind").value;
  const slot = $("blTargetSlot");
  pickedNodeId = null;
  if (kind === "inventory_item") {
    slot.innerHTML = `<select id="blInv"><option value="">loading…</option></select>`;
    invoke("maker_list_inventory", a())
      .then((items) => {
        $("blInv").innerHTML =
          `<option value="">Pick an item…</option>` +
          items.map((i) => `<option value="${i.id}">${esc(i.name)}</option>`).join("");
      })
      .catch(() => {
        $("blInv").innerHTML = `<option value="">(none — add inventory first)</option>`;
      });
  } else if (kind === "design_file") {
    slot.innerHTML = `<button id="blPick" type="button">Pick file…</button><span id="blPicked" class="muted"></span>`;
    $("blPick").onclick = async () => {
      const picked = await pickFile("Pick a design file for this line");
      if (picked) {
        pickedNodeId = picked.node_id;
        $("blPicked").textContent = ` ${picked.name}`;
      }
    };
  } else {
    slot.innerHTML = `<input id="blText" placeholder="Part name" class="bl-text" />`;
  }
}

async function addBomLine() {
  const kind = $("blKind").value;
  const input = {
    reference: $("blRef").value.trim() || null,
    quantity: Number($("blQty").value) || 1,
    unit: $("blUnit").value,
  };
  if (kind === "inventory_item") {
    const id = Number($("blInv").value);
    if (!id) {
      $("projErr").textContent = "Pick an inventory item.";
      return;
    }
    input.inventory_item_id = id;
  } else if (kind === "design_file") {
    if (!pickedNodeId) {
      $("projErr").textContent = "Pick a design file.";
      return;
    }
    input.node_id = pickedNodeId;
  } else {
    const name = $("blText").value.trim();
    if (!name) {
      $("projErr").textContent = "Enter a part name.";
      return;
    }
    input.part_name = name;
  }
  $("projErr").textContent = "";
  try {
    await invoke("maker_create_bom_line", { ...a(), bomId: currentBomId, input });
    loadRollup(currentBomId);
  } catch (e) {
    $("projErr").textContent = String(e);
  }
}

// ---- lazy load on first show -------------------------------------------------

let loaded = false;
window.addEventListener("view:show", (e) => {
  if (e.detail === "projects") {
    loadProjects();
    loaded = true;
  }
});
