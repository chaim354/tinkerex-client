// Inventory surface: items list, stock adjust (+/-), low-stock view, locations &
// suppliers.
//
// Tauri commands:
//   maker_list_inventory {…,category?,locationId?}  maker_low_stock {…}
//   maker_get_inventory_item {…,id}
//   maker_create_inventory_item/maker_update_inventory_item {…,input}
//   maker_delete_inventory_item {…,id}
//   maker_adjust_inventory {…,id,delta,reason?,note?}
//   maker_inventory_transactions {…,id}
//   maker_list_locations/maker_create_location/maker_delete_location
//   maker_list_suppliers/maker_create_supplier/maker_delete_supplier

import { esc, showOverlay, closeOverlay, wireOverlay } from "/maker-shared.js";

const UI = window.MakerUI;
const { invoke, $ } = UI;
const a = () => UI.args();

const UNITS = ["each", "g", "kg", "m", "mm", "cm", "ml", "l", "pack", "roll", "sheet", "other"];

let lowStockOnly = false;
let locationsCache = [];
let suppliersCache = [];

// ---- list --------------------------------------------------------------------

async function loadInventory() {
  // See library.js#loadCatalog.
  if (!UI.signedIn()) {
    $("invList").innerHTML = `<div class="muted">${esc(UI.SIGNED_OUT_HINT)}</div>`;
    return;
  }
  let items;
  try {
    items = lowStockOnly
      ? await invoke("maker_low_stock", a())
      : await invoke("maker_list_inventory", a());
  } catch (e) {
    $("invList").innerHTML = `<div class="err">${esc(String(e))}</div>`;
    return;
  }
  const filter = $("invSearch").value.trim().toLowerCase();
  if (filter) items = items.filter((i) => i.name.toLowerCase().includes(filter));
  renderInventory(items);
}

function renderInventory(items) {
  const host = $("invList");
  host.innerHTML = "";
  $("invEmpty").classList.toggle("hidden", items.length > 0);
  for (const it of items) {
    const low = it.quantity_cached <= it.min_quantity;
    const row = document.createElement("div");
    row.className = "inv-row" + (low ? " low" : "");
    row.innerHTML = `
      <span class="inv-name" role="button" tabindex="0">${esc(it.name)}</span>
      <span class="inv-cat muted">${esc(it.category || "")}</span>
      <span class="inv-qty${low ? " low" : ""}">${it.quantity_cached} ${esc(it.unit)}</span>
      <span class="inv-min muted">min ${it.min_quantity}</span>
    `;
    row.querySelector(".inv-name").onclick = () => openItem(it.id);

    const minus = document.createElement("button");
    minus.className = "adj";
    minus.textContent = "−";
    minus.title = "Decrease stock";
    minus.onclick = () => adjust(it.id, -1);
    const plus = document.createElement("button");
    plus.className = "adj";
    plus.textContent = "+";
    plus.title = "Increase stock";
    plus.onclick = () => adjust(it.id, +1);
    row.appendChild(minus);
    row.appendChild(plus);
    host.appendChild(row);
  }
}

async function adjust(id, delta) {
  // The server only allows reasons: consumed / restocked / adjusted / initial.
  const reason = delta >= 0 ? "restocked" : "consumed";
  try {
    await invoke("maker_adjust_inventory", { ...a(), id, delta, reason });
    loadInventory();
  } catch (e) {
    UI.toast($("connToast"), String(e), "err");
  }
}

$("invSearch").addEventListener("input", loadInventory);
$("invLowStock").onclick = () => {
  lowStockOnly = !lowStockOnly;
  $("invLowStock").setAttribute("aria-pressed", String(lowStockOnly));
  $("invLowStock").classList.toggle("active", lowStockOnly);
  loadInventory();
};

// ---- create ------------------------------------------------------------------

$("invNew").onclick = () => openItem(null);

// ---- detail / edit overlay ---------------------------------------------------

let currentItem = null;
const closeInv = () => {
  currentItem = null;
  closeOverlay("invOverlay");
  $("invErr").textContent = "";
};
wireOverlay("invOverlay", "invClose", closeInv);

async function openItem(id) {
  $("invErr").textContent = "";
  // load locations/suppliers for the dropdowns (cached).
  try {
    [locationsCache, suppliersCache] = await Promise.all([
      invoke("maker_list_locations", a()),
      invoke("maker_list_suppliers", a()),
    ]);
  } catch {
    locationsCache = [];
    suppliersCache = [];
  }

  if (id == null) {
    currentItem = null;
    $("invTitle").textContent = "New inventory item";
    renderItemForm({ name: "", unit: "each", min_quantity: 0, quantity_cached: 0 });
    showOverlay("invOverlay");
    return;
  }
  $("invTitle").textContent = "Item";
  $("invBody").innerHTML = `<div class="muted"><span class="spinner"></span>loading…</div>`;
  showOverlay("invOverlay");
  try {
    currentItem = await invoke("maker_get_inventory_item", { ...a(), id });
  } catch (e) {
    $("invErr").textContent = String(e);
    return;
  }
  $("invTitle").textContent = currentItem.name;
  renderItemForm(currentItem);
}

function optList(rows, selected) {
  return (
    `<option value="">—</option>` +
    rows
      .map((r) => `<option value="${r.id}"${r.id === selected ? " selected" : ""}>${esc(r.name)}</option>`)
      .join("")
  );
}

function renderItemForm(it) {
  const unitOpts = UNITS.map(
    (u) => `<option${u === it.unit ? " selected" : ""}>${u}</option>`,
  ).join("");
  $("invBody").innerHTML = `
    <div class="form-grid">
      <label>Name<input id="ivName" value="${esc(it.name)}" /></label>
      <label>Part #<input id="ivPart" value="${esc(it.part_number || "")}" /></label>
      <label>Category<input id="ivCat" value="${esc(it.category || "")}" /></label>
      <label>Unit<select id="ivUnit">${unitOpts}</select></label>
      <label>Min quantity<input id="ivMin" type="number" min="0" value="${it.min_quantity ?? 0}" /></label>
      <label>Unit cost<input id="ivCost" value="${esc(it.unit_cost || "")}" placeholder="0.00" /></label>
      <label>Location<select id="ivLoc">${optList(locationsCache, it.location_id)}</select></label>
      <label>Supplier<select id="ivSup">${optList(suppliersCache, it.supplier_id)}</select></label>
      <label class="full">Notes<textarea id="ivNotes" rows="2">${esc(it.notes || "")}</textarea></label>
    </div>
    <div class="row">
      <button id="ivSave" class="primary">Save</button>
      ${currentItem ? `<button id="ivDelete" class="danger">Delete</button>` : ""}
    </div>
    ${currentItem ? renderAdjustBlock(it) : ""}
  `;
  $("ivSave").onclick = saveItem;
  if (currentItem) {
    $("ivDelete").onclick = deleteItem;
    $("ivAdjustApply").onclick = applyAdjust;
    $("ivLedgerBtn").onclick = loadLedger;
  }
}

function renderAdjustBlock(it) {
  return `
    <h4>Stock: <b>${it.quantity_cached} ${esc(it.unit)}</b></h4>
    <div class="row" style="margin-top:0">
      <input id="ivAdjustDelta" type="number" step="1" value="1" class="bl-qty" />
      <select id="ivAdjustReason">
        <option value="adjusted">adjusted</option>
        <option value="restocked">restocked</option>
        <option value="consumed">consumed</option>
        <option value="initial">initial</option>
      </select>
      <button id="ivAdjustApply">Apply adjustment</button>
    </div>
    <div class="row" style="margin-top:0"><button id="ivLedgerBtn" class="link">Show ledger</button></div>
    <div id="ivLedger" class="log-list"></div>
  `;
}

function itemInput() {
  return {
    name: $("ivName").value.trim(),
    part_number: $("ivPart").value.trim() || null,
    category: $("ivCat").value.trim() || null,
    unit: $("ivUnit").value,
    min_quantity: Number($("ivMin").value) || 0,
    unit_cost: $("ivCost").value.trim() || null,
    location_id: Number($("ivLoc").value) || null,
    supplier_id: Number($("ivSup").value) || null,
    notes: $("ivNotes").value || null,
  };
}

async function saveItem() {
  const input = itemInput();
  if (!input.name) {
    $("invErr").textContent = "Name is required.";
    return;
  }
  await UI.withBusy($("ivSave"), "Saving…", async () => {
    try {
      if (currentItem) {
        currentItem = await invoke("maker_update_inventory_item", { ...a(), id: currentItem.id, input });
      } else {
        currentItem = await invoke("maker_create_inventory_item", { ...a(), input });
        $("invTitle").textContent = currentItem.name;
        renderItemForm(currentItem); // reveal the adjust block now that it has an id
      }
      loadInventory();
    } catch (e) {
      $("invErr").textContent = String(e);
    }
  });
}

async function deleteItem() {
  if (!window.confirm(`Delete “${currentItem.name}”?`)) return;
  try {
    await invoke("maker_delete_inventory_item", { ...a(), id: currentItem.id });
    closeInv();
    loadInventory();
  } catch (e) {
    $("invErr").textContent = String(e);
  }
}

async function applyAdjust() {
  const delta = Number($("ivAdjustDelta").value);
  if (!delta) {
    $("invErr").textContent = "Enter a non-zero delta.";
    return;
  }
  try {
    currentItem = await invoke("maker_adjust_inventory", {
      ...a(),
      id: currentItem.id,
      delta,
      reason: $("ivAdjustReason").value || "adjusted",
    });
    renderItemForm(currentItem);
    loadInventory();
  } catch (e) {
    $("invErr").textContent = String(e);
  }
}

async function loadLedger() {
  $("ivLedger").innerHTML = `<div class="muted"><span class="spinner"></span>loading ledger…</div>`;
  try {
    const txns = await invoke("maker_inventory_transactions", { ...a(), id: currentItem.id });
    $("ivLedger").innerHTML = txns.length
      ? txns
          .map(
            (t) =>
              `<div class="log-entry"><span class="badge">${t.delta > 0 ? "+" : ""}${t.delta}</span>` +
              `<span class="log-body">${esc(t.reason)}${t.note ? " — " + esc(t.note) : ""}</span>` +
              `<span class="log-when muted">${t.created_at ? UI.relWhen(t.created_at) : ""}</span></div>`,
          )
          .join("")
      : `<div class="muted">No transactions yet.</div>`;
  } catch (e) {
    $("ivLedger").innerHTML = `<div class="err">${esc(String(e))}</div>`;
  }
}

// ---- locations & suppliers ---------------------------------------------------

async function loadLocations() {
  try {
    locationsCache = await invoke("maker_list_locations", a());
  } catch {
    locationsCache = [];
  }
  renderChips("locList", locationsCache, "maker_delete_location");
}

async function loadSuppliers() {
  try {
    suppliersCache = await invoke("maker_list_suppliers", a());
  } catch {
    suppliersCache = [];
  }
  renderChips("supList", suppliersCache, "maker_delete_supplier");
}

function renderChips(hostId, rows, deleteCmd) {
  const host = $(hostId);
  host.innerHTML = "";
  if (!rows.length) {
    host.innerHTML = `<div class="muted">None yet.</div>`;
    return;
  }
  for (const r of rows) {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.textContent = r.name;
    const x = document.createElement("button");
    x.className = "chip-x";
    x.setAttribute("aria-label", "Remove");
    x.textContent = "×";
    x.onclick = async () => {
      try {
        await invoke(deleteCmd, { ...a(), id: r.id });
        if (deleteCmd === "maker_delete_location") loadLocations();
        else loadSuppliers();
      } catch (e) {
        UI.toast($("connToast"), String(e), "err");
      }
    };
    chip.appendChild(x);
    host.appendChild(chip);
  }
}

$("locCreate").onclick = async () => {
  const name = $("locName").value.trim();
  if (!name) return;
  try {
    await invoke("maker_create_location", { ...a(), name });
    $("locName").value = "";
    loadLocations();
  } catch (e) {
    UI.toast($("connToast"), String(e), "err");
  }
};

$("supCreate").onclick = async () => {
  const name = $("supName").value.trim();
  if (!name) return;
  try {
    await invoke("maker_create_supplier", { ...a(), name, url: $("supUrl").value.trim() || null });
    $("supName").value = "";
    $("supUrl").value = "";
    loadSuppliers();
  } catch (e) {
    UI.toast($("connToast"), String(e), "err");
  }
};

// ---- lazy load on first show -------------------------------------------------

window.addEventListener("view:show", (e) => {
  if (e.detail === "inventory") {
    loadInventory();
    loadLocations();
    loadSuppliers();
  }
});
