//! Typed client for the **Maker Library** API (`/v1/maker/*`) — the product/catalog
//! layer that sits *over* the file-sync engine.
//!
//! These records are **server-authoritative** (normal Rails integer-PK CRUD), *not*
//! synced into the local SQLite tree. They reference synced files only by the file
//! layer's stable string `node_id` (+ optional `version_id`). The wire shapes here
//! mirror the controllers in `app/controllers/v1/maker/*` exactly.
//!
//! Style matches the rest of [`ServerClient`](crate::ServerClient): blocking
//! `reqwest`, `self.authed(rb)`, `Self::json(resp)`, and `Result`/`Error` from the
//! crate root. The methods are added to `ServerClient` via an `impl` block here.

use serde::{Deserialize, Serialize};

use crate::{Result, ServerClient};

/// Deserialize a Rails `decimal` column into `f64`. Rails serializes `BigDecimal` as a
/// JSON **string** (e.g. `"50.0"`) in these controllers, but a plain number (`50.0`) is
/// also accepted so the types are robust to either rendering. Used for inventory
/// quantities and BOM line quantities.
fn de_decimal<'de, D>(d: D) -> std::result::Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrNum {
        Str(String),
        Num(f64),
    }
    match StrOrNum::deserialize(d)? {
        StrOrNum::Num(n) => Ok(n),
        StrOrNum::Str(s) => s.parse::<f64>().map_err(serde::de::Error::custom),
    }
}

// ---- items ---------------------------------------------------------------------

/// An item as it appears in the list (`GET /v1/maker/items`) — the "short" shape the
/// controller renders for `index`. The `description`/`instructions`/`file_attachments`/
/// `tags` are only present on the full view (see [`ItemDetail`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub item_type: String,
    pub origin: String,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub source_name: Option<String>,
    #[serde(default)]
    pub primary_node_id: Option<String>,
    pub favorited: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// A file attached to an item by role (the file-layer seam). `node_id` (+ optional
/// `version_id`) points into the synced tree — the UI reuses the file browser to pick it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileAttachment {
    pub id: i64,
    pub node_id: String,
    #[serde(default)]
    pub version_id: Option<String>,
    pub role: String,
    #[serde(default)]
    pub label: Option<String>,
    pub position: i64,
}

/// A tagging on an item's full view (`taggings: [{id, tag_id, name}]`). The `id` is the
/// **tagging** id (the join row) — the key `DELETE /v1/maker/taggings/:id` (and the
/// `untag` client method) needs to remove a tag from an item. `tag_id`/`name` identify
/// the underlying tag. (The item view also still returns plain `tags: [names]` for display.)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaggingDetail {
    pub id: i64,
    pub tag_id: i64,
    pub name: String,
}

/// The full item view (`GET /v1/maker/items/:id`, and the body returned by
/// create/update): the short shape plus description/instructions/attachments/tags.
///
/// Also `Serialize` so a Tauri command can return it to the JS frontend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemDetail {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub item_type: String,
    pub origin: String,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub source_name: Option<String>,
    #[serde(default)]
    pub primary_node_id: Option<String>,
    pub favorited: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub file_attachments: Vec<FileAttachment>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The item's taggings with their join-row ids — for in-app tag *removal*
    /// (`untag(tagging.id)`). The plain `tags` above stays for display/back-compat.
    #[serde(default)]
    pub taggings: Vec<TaggingDetail>,
}

/// The mutable fields of an item — used to build the `{ item: { … } }` body for
/// create/update. Every field is optional so `update_item` can send a partial patch;
/// `None` fields are omitted from the JSON (Rails leaves unpermitted/absent keys
/// untouched on update). `create_item` should at least set `title`.
///
/// Also `Deserialize` so it can arrive as a Tauri command arg from the JS frontend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favorited: Option<bool>,
}

impl ItemInput {
    /// Convenience: a fresh item with just a title (the only required field).
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ItemsResponse {
    pub items: Vec<Item>,
}

/// The body for attaching a file to an item (`POST /items/:id/file_attachments`).
///
/// Also `Deserialize` so it can arrive as a Tauri command arg from the JS frontend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileAttachmentInput {
    pub node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
}

// ---- tags ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TagsResponse {
    pub tags: Vec<Tag>,
}

/// The response of tagging an item (`POST /items/:id/taggings`) — the (normalized) tag name.
#[derive(Debug, Deserialize)]
pub struct TaggingResponse {
    pub tag: String,
}

// ---- collections ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub item_count: i64,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CollectionsResponse {
    pub collections: Vec<Collection>,
}

/// An item as listed inside a collection (`GET /collections/:id/items`) — the controller
/// renders just id/title/item_type there.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectionItem {
    pub id: i64,
    pub title: String,
    pub item_type: String,
}

#[derive(Debug, Deserialize)]
pub struct CollectionItemsResponse {
    pub items: Vec<CollectionItem>,
}

// ---- tasks ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub notes: Option<String>,
    /// `"todo"` | `"doing"` | `"done"`.
    pub status: String,
    pub position: i64,
    #[serde(default)]
    pub completed_at: Option<String>,
    /// Optional polymorphic owner (e.g. `"Maker::Item"`) — `None` for a standalone task.
    #[serde(default)]
    pub taskable_type: Option<String>,
    #[serde(default)]
    pub taskable_id: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TasksResponse {
    pub tasks: Vec<Task>,
}

/// The mutable fields of a task — the `{ task: { … } }` body for create/update. All
/// optional for partial patches; `create_task` should set at least `title`. Pair
/// `taskable_type` + `taskable_id` to attach the task to an item (omit both for standalone).
///
/// Also `Deserialize` so it can arrive as a Tauri command arg from the JS frontend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taskable_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taskable_id: Option<i64>,
}

impl TaskInput {
    /// Convenience: a fresh standalone task with just a title.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            ..Default::default()
        }
    }
}

// ---- projects ------------------------------------------------------------------

/// A build-log entry on a project (`build_log_entries: [...]` in the full project view,
/// or returned by `POST .../build_log_entries`). `entry_type` is a free string the
/// controller permits (e.g. `note`, `milestone`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BuildLogEntry {
    pub id: i64,
    pub body: String,
    #[serde(default)]
    pub entry_type: Option<String>,
    #[serde(default)]
    pub logged_at: Option<String>,
}

/// A project. The short shape (index) carries id/name/slug/status/timestamps; the full
/// shape (show/create/update) additionally carries description + build_log_entries +
/// file_attachments. `serde(default)` makes the full-only fields tolerant of the short body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub slug: Option<String>,
    pub status: String,
    #[serde(default)]
    pub archived_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub build_log_entries: Vec<BuildLogEntry>,
    #[serde(default)]
    pub file_attachments: Vec<FileAttachment>,
}

#[derive(Debug, Deserialize)]
pub struct ProjectsResponse {
    pub projects: Vec<Project>,
}

/// Mutable project fields (`{ project: { … } }` body). Partial-friendly: `None` fields omitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

impl ProjectInput {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..Default::default()
        }
    }
}

/// The body for adding a build-log entry (`{ build_log_entry: { … } }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildLogEntryInput {
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logged_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BuildLogEntriesResponse {
    pub build_log_entries: Vec<BuildLogEntry>,
}

// ---- inventory -----------------------------------------------------------------

/// An inventory item. Short fields always present; `unit_cost`/`notes`/`created_at` only on
/// the full view (show/create/update/adjust). `unit_cost` is a decimal serialized as a string
/// by Rails — kept as `Option<String>` to avoid float rounding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InventoryItem {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub part_number: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    pub unit: String,
    /// Rails decimals (precision 12, scale 3) → JSON **strings** like `"50.0"` (may be
    /// fractional); [`de_decimal`] also tolerates a plain number.
    #[serde(deserialize_with = "de_decimal")]
    pub quantity_cached: f64,
    #[serde(deserialize_with = "de_decimal")]
    pub min_quantity: f64,
    #[serde(default)]
    pub location_id: Option<i64>,
    #[serde(default)]
    pub supplier_id: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub unit_cost: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InventoryItemsResponse {
    pub inventory_items: Vec<InventoryItem>,
}

/// Mutable inventory fields (`{ inventory_item: { … } }`). Partial-friendly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InventoryItemInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_quantity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_cost: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supplier_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// One row of an item's stock ledger (`GET .../transactions`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InventoryTransaction {
    pub id: i64,
    /// Rails decimal → JSON string like `"120.0"`; [`de_decimal`] parses it.
    #[serde(deserialize_with = "de_decimal")]
    pub delta: f64,
    pub reason: String,
    #[serde(default)]
    pub project_id: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InventoryTransactionsResponse {
    pub transactions: Vec<InventoryTransaction>,
}

/// A storage location (optionally nested via `parent_id`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Location {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LocationsResponse {
    pub locations: Vec<Location>,
}

/// A supplier (vendor) record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Supplier {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SuppliersResponse {
    pub suppliers: Vec<Supplier>,
}

// ---- BOM -----------------------------------------------------------------------

/// A bill-of-materials (one revision) belonging to a project. `line_count` only on the
/// full view (`show`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bom {
    pub id: i64,
    pub project_id: i64,
    #[serde(default)]
    pub name: Option<String>,
    pub revision: i64,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub line_count: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct BomsResponse {
    pub boms: Vec<Bom>,
}

/// The body to create/update a BOM (`{ bom: { name?, revision? } }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BomInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
}

/// A BOM line (the response from create/update/list). `quantity` is a Rails decimal
/// serialized as a string; `target_kind` is one of `inventory_item`/`design_file`/
/// `free_text` (or null if unresolved). Exactly one of inventory_item_id/node_id/part_name
/// is set (server-enforced).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BomLine {
    pub id: i64,
    pub bom_id: i64,
    #[serde(default)]
    pub reference: Option<String>,
    /// Rails decimal → JSON string like `"4.0"` (may be fractional).
    #[serde(deserialize_with = "de_decimal")]
    pub quantity: f64,
    pub unit: String,
    #[serde(default)]
    pub notes: Option<String>,
    pub position: i64,
    #[serde(default)]
    pub inventory_item_id: Option<i64>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub part_name: Option<String>,
    #[serde(default)]
    pub supplier_id: Option<i64>,
    #[serde(default)]
    pub target_kind: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BomLinesResponse {
    pub bom_lines: Vec<BomLine>,
}

/// The body to create/update a BOM line. Set exactly one target
/// (`inventory_item_id` | `node_id` | `part_name`) — the server validates it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BomLineInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory_item_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supplier_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
}

/// One roll-up line: the line fields plus its `target` resolved to a free-form JSON object
/// (shape depends on `target_kind` — see [`BomLine`]/the controller's `resolved_target`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RollupLine {
    pub id: i64,
    #[serde(default)]
    pub reference: Option<String>,
    /// Rails decimal → JSON string like `"4.0"` (may be fractional).
    #[serde(deserialize_with = "de_decimal")]
    pub quantity: f64,
    pub unit: String,
    #[serde(default)]
    pub notes: Option<String>,
    pub position: i64,
    #[serde(default)]
    pub target_kind: Option<String>,
    /// The resolved target, rendered per-kind by the UI (kept as opaque JSON).
    pub target: serde_json::Value,
}

/// `GET /v1/maker/boms/:id/rollup` — the BOM header plus every line with its resolved target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BomRollup {
    pub bom: Bom,
    pub lines: Vec<RollupLine>,
}

// ---- previews ------------------------------------------------------------------

/// A preview (thumbnail / model_glb / render) for a node+version. `url` is the blob URL
/// (bytes off Rails); it's `null` until `status == "ready"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Preview {
    pub id: i64,
    pub node_id: String,
    #[serde(default)]
    pub version_id: Option<String>,
    pub kind: String,
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub width: Option<i64>,
    #[serde(default)]
    pub height: Option<i64>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PreviewsResponse {
    pub previews: Vec<Preview>,
}

/// The body for `POST /v1/maker/previews` (`{ preview: { … } }`).
///
/// A DERIVED STRUCT RATHER THAN AN INLINE `serde_json::json!`, AND THAT IS THE WHOLE POINT:
/// a `json!` literal has nowhere to hang `skip_serializing_if`, so every `None` went out as
/// an explicit `null` — `{"filename":null,"kind":null,"node_id":"…","version_id":null}`.
///
/// The server distinguishes an ABSENT key from an explicit `null` and is right to:
/// `attrs.fetch(:kind, "thumbnail")` defaults only on absence, so a `null` reached the model
/// and the `kind` inclusion validation refused it — **422**, zero rows. That is the correct
/// answer to a client that named the key and supplied nothing (reject, don't normalize —
/// CLAUDE.md §14a's `key_for`, §14c's `validate_asset_path!` and the SDK's path-segment
/// rule all take the same side), so the fix belongs on THIS end: don't send the key.
///
/// Reachable, not theoretical: `maker_enqueue_preview` is a Tauri command whose `kind` is an
/// `Option<String>`, so any JS caller omitting it produced a request that could never succeed.
///
/// Borrowed rather than owned so `enqueue_preview`'s `&str` signature is unchanged and the
/// body costs no allocations. Every other request body in this file — `ItemInput`,
/// `FileAttachmentInput`, `InventoryItemInput`, `BomInput` — is already a derived struct
/// carrying these attributes; the preview door was the crate's lone exception to its own
/// typed-input pattern.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PreviewInput<'a> {
    pub node_id: &'a str,
    /// Omitted when `None`. Absent and `null` are NOT equivalent to the server here either:
    /// `refuse_stale_version!` gates on `attrs[:version_id].blank?`, which is true for both,
    /// so omitting is a no-op for that check — it is `kind` that the omission rescues.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<&'a str>,
}

// ---- the API methods ------------------------------------------------------------

impl ServerClient {
    // -- items --

    /// List catalog items, newest-updated first. Each filter is optional and maps to the
    /// `?type=&tag=&q=` query the controller's `index` understands (type = `item_type`,
    /// tag = a tag *name*, q = case-insensitive title/summary/description substring).
    pub fn list_items(
        &self,
        type_: Option<&str>,
        tag: Option<&str>,
        q: Option<&str>,
    ) -> Result<Vec<Item>> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(t) = type_ {
            query.push(("type", t.to_string()));
        }
        if let Some(t) = tag {
            query.push(("tag", t.to_string()));
        }
        if let Some(t) = q {
            query.push(("q", t.to_string()));
        }
        let resp = self
            .authed(self.http.get(self.url("/v1/maker/items")))?
            .query(&query)
            .send()?;
        let r: ItemsResponse = Self::json(resp)?;
        Ok(r.items)
    }

    /// Create a catalog item. Returns the full view (the controller renders `full: true`).
    pub fn create_item(&self, input: &ItemInput) -> Result<ItemDetail> {
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/items")))?
            .json(&serde_json::json!({ "item": input }))
            .send()?;
        Self::json(resp)
    }

    /// Fetch one item's full view (description/instructions/attachments/tags).
    pub fn get_item(&self, id: i64) -> Result<ItemDetail> {
        let resp = self
            .authed(self.http.get(self.url(&format!("/v1/maker/items/{id}"))))?
            .send()?;
        Self::json(resp)
    }

    /// Patch an item (only the `Some` fields of `input` are sent). Returns the full view.
    pub fn update_item(&self, id: i64, input: &ItemInput) -> Result<ItemDetail> {
        let resp = self
            .authed(self.http.patch(self.url(&format!("/v1/maker/items/{id}"))))?
            .json(&serde_json::json!({ "item": input }))
            .send()?;
        Self::json(resp)
    }

    /// Delete an item (cascades its attachments/taggings/collection memberships server-side).
    pub fn delete_item(&self, id: i64) -> Result<()> {
        let resp = self
            .authed(self.http.delete(self.url(&format!("/v1/maker/items/{id}"))))?
            .send()?;
        Self::ok_no_content(resp)
    }

    // -- file attachments --

    /// Attach a synced file (by `node_id`) to an item with a role. Returns the new attachment.
    pub fn attach_file(&self, item_id: i64, input: &FileAttachmentInput) -> Result<FileAttachment> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v1/maker/items/{item_id}/file_attachments"))),
            )?
            .json(&serde_json::json!({ "file_attachment": input }))
            .send()?;
        Self::json(resp)
    }

    /// Detach a file from its item (shallow route — keyed by the *attachment* id).
    pub fn detach_file(&self, attachment_id: i64) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .delete(self.url(&format!("/v1/maker/file_attachments/{attachment_id}"))),
            )?
            .send()?;
        Self::ok_no_content(resp)
    }

    // -- tags & taggings --

    /// Tag an item by tag *name* (the tag is created on the fly if new). Returns the
    /// normalized tag name the server stored.
    pub fn tag_item(&self, item_id: i64, name: &str) -> Result<String> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v1/maker/items/{item_id}/taggings"))),
            )?
            .json(&serde_json::json!({ "name": name }))
            .send()?;
        let r: TaggingResponse = Self::json(resp)?;
        Ok(r.tag)
    }

    /// Remove a tagging (shallow route — keyed by the *tagging* id, not the tag id).
    pub fn untag(&self, tagging_id: i64) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .delete(self.url(&format!("/v1/maker/taggings/{tagging_id}"))),
            )?
            .send()?;
        Self::ok_no_content(resp)
    }

    /// All of the user's tags (for the tag filter chips / autocomplete).
    pub fn list_tags(&self) -> Result<Vec<Tag>> {
        let resp = self
            .authed(self.http.get(self.url("/v1/maker/tags")))?
            .send()?;
        let r: TagsResponse = Self::json(resp)?;
        Ok(r.tags)
    }

    // -- collections --

    /// All of the user's collections (name + item_count), alphabetically.
    pub fn list_collections(&self) -> Result<Vec<Collection>> {
        let resp = self
            .authed(self.http.get(self.url("/v1/maker/collections")))?
            .send()?;
        let r: CollectionsResponse = Self::json(resp)?;
        Ok(r.collections)
    }

    /// Create a collection.
    pub fn create_collection(&self, name: &str, description: Option<&str>) -> Result<Collection> {
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/collections")))?
            .json(
                &serde_json::json!({ "collection": { "name": name, "description": description } }),
            )
            .send()?;
        Self::json(resp)
    }

    /// The items in a collection (id/title/item_type each).
    pub fn collection_items(&self, collection_id: i64) -> Result<Vec<CollectionItem>> {
        let resp = self
            .authed(
                self.http
                    .get(self.url(&format!("/v1/maker/collections/{collection_id}/items"))),
            )?
            .send()?;
        let r: CollectionItemsResponse = Self::json(resp)?;
        Ok(r.items)
    }

    /// Add an item to a collection (idempotent server-side — `find_or_create_by`).
    pub fn add_to_collection(&self, collection_id: i64, item_id: i64) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v1/maker/collections/{collection_id}/items"))),
            )?
            .json(&serde_json::json!({ "item_id": item_id }))
            .send()?;
        Self::ok_no_content(resp)
    }

    /// Remove an item from a collection.
    pub fn remove_from_collection(&self, collection_id: i64, item_id: i64) -> Result<()> {
        let resp = self
            .authed(self.http.delete(self.url(&format!(
                "/v1/maker/collections/{collection_id}/items/{item_id}"
            ))))?
            .send()?;
        Self::ok_no_content(resp)
    }

    // -- tasks --

    /// List tasks (by position then creation). `open_only` maps to `?open=true` —
    /// excludes `done` tasks (the board's "active" view).
    pub fn list_tasks(&self, open_only: bool) -> Result<Vec<Task>> {
        let mut rb = self.authed(self.http.get(self.url("/v1/maker/tasks")))?;
        if open_only {
            rb = rb.query(&[("open", "true")]);
        }
        let r: TasksResponse = Self::json(rb.send()?)?;
        Ok(r.tasks)
    }

    /// Create a task (standalone, or attached to an item via `TaskInput::taskable_*`).
    pub fn create_task(&self, input: &TaskInput) -> Result<Task> {
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/tasks")))?
            .json(&serde_json::json!({ "task": input }))
            .send()?;
        Self::json(resp)
    }

    /// Patch a task — e.g. move it across the board (`status`) or complete it (status
    /// `"done"` stamps `completed_at` server-side).
    pub fn update_task(&self, id: i64, input: &TaskInput) -> Result<Task> {
        let resp = self
            .authed(self.http.patch(self.url(&format!("/v1/maker/tasks/{id}"))))?
            .json(&serde_json::json!({ "task": input }))
            .send()?;
        Self::json(resp)
    }

    /// Delete a task.
    pub fn delete_task(&self, id: i64) -> Result<()> {
        let resp = self
            .authed(self.http.delete(self.url(&format!("/v1/maker/tasks/{id}"))))?
            .send()?;
        Self::ok_no_content(resp)
    }

    // -- projects --

    /// List projects (newest-updated first). `status` filters via `?status=`.
    pub fn list_projects(&self, status: Option<&str>) -> Result<Vec<Project>> {
        let mut rb = self.authed(self.http.get(self.url("/v1/maker/projects")))?;
        if let Some(s) = status {
            rb = rb.query(&[("status", s)]);
        }
        let r: ProjectsResponse = Self::json(rb.send()?)?;
        Ok(r.projects)
    }

    /// One project's full view (description + build log + attachments).
    pub fn get_project(&self, id: i64) -> Result<Project> {
        let resp = self
            .authed(self.http.get(self.url(&format!("/v1/maker/projects/{id}"))))?
            .send()?;
        Self::json(resp)
    }

    /// Create a project. Returns the full view.
    pub fn create_project(&self, input: &ProjectInput) -> Result<Project> {
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/projects")))?
            .json(&serde_json::json!({ "project": input }))
            .send()?;
        Self::json(resp)
    }

    /// Patch a project.
    pub fn update_project(&self, id: i64, input: &ProjectInput) -> Result<Project> {
        let resp = self
            .authed(
                self.http
                    .patch(self.url(&format!("/v1/maker/projects/{id}"))),
            )?
            .json(&serde_json::json!({ "project": input }))
            .send()?;
        Self::json(resp)
    }

    /// Delete a project.
    pub fn delete_project(&self, id: i64) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .delete(self.url(&format!("/v1/maker/projects/{id}"))),
            )?
            .send()?;
        Self::ok_no_content(resp)
    }

    // -- build-log entries --

    /// All build-log entries for a project.
    pub fn list_build_log(&self, project_id: i64) -> Result<Vec<BuildLogEntry>> {
        let resp = self
            .authed(self.http.get(self.url(&format!(
                "/v1/maker/projects/{project_id}/build_log_entries"
            ))))?
            .send()?;
        let r: BuildLogEntriesResponse = Self::json(resp)?;
        Ok(r.build_log_entries)
    }

    /// Add a build-log entry to a project.
    pub fn add_build_log(
        &self,
        project_id: i64,
        input: &BuildLogEntryInput,
    ) -> Result<BuildLogEntry> {
        let resp = self
            .authed(self.http.post(self.url(&format!(
                "/v1/maker/projects/{project_id}/build_log_entries"
            ))))?
            .json(&serde_json::json!({ "build_log_entry": input }))
            .send()?;
        Self::json(resp)
    }

    /// Delete a build-log entry (shallow route — keyed by the entry id).
    pub fn delete_build_log(&self, entry_id: i64) -> Result<()> {
        let resp = self
            .authed(self.http.delete(self.url(&format!(
                "/v1/maker/build_log_entries/{entry_id}"
            ))))?
            .send()?;
        Self::ok_no_content(resp)
    }

    /// Attach a synced file (by `node_id`) to a project.
    pub fn attach_file_to_project(
        &self,
        project_id: i64,
        input: &FileAttachmentInput,
    ) -> Result<FileAttachment> {
        let resp = self
            .authed(self.http.post(self.url(&format!(
                "/v1/maker/projects/{project_id}/file_attachments"
            ))))?
            .json(&serde_json::json!({ "file_attachment": input }))
            .send()?;
        Self::json(resp)
    }

    // -- inventory --

    /// List inventory items (alpha). Optional `category`/`location_id` filters.
    pub fn list_inventory(
        &self,
        category: Option<&str>,
        location_id: Option<i64>,
    ) -> Result<Vec<InventoryItem>> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(c) = category {
            query.push(("category", c.to_string()));
        }
        if let Some(l) = location_id {
            query.push(("location_id", l.to_string()));
        }
        let resp = self
            .authed(self.http.get(self.url("/v1/maker/inventory_items")))?
            .query(&query)
            .send()?;
        let r: InventoryItemsResponse = Self::json(resp)?;
        Ok(r.inventory_items)
    }

    /// Items at or below their `min_quantity`.
    pub fn list_low_stock(&self) -> Result<Vec<InventoryItem>> {
        let resp = self
            .authed(
                self.http
                    .get(self.url("/v1/maker/inventory_items/low_stock")),
            )?
            .send()?;
        let r: InventoryItemsResponse = Self::json(resp)?;
        Ok(r.inventory_items)
    }

    /// One inventory item's full view.
    pub fn get_inventory_item(&self, id: i64) -> Result<InventoryItem> {
        let resp = self
            .authed(
                self.http
                    .get(self.url(&format!("/v1/maker/inventory_items/{id}"))),
            )?
            .send()?;
        Self::json(resp)
    }

    /// Create an inventory item.
    pub fn create_inventory_item(&self, input: &InventoryItemInput) -> Result<InventoryItem> {
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/inventory_items")))?
            .json(&serde_json::json!({ "inventory_item": input }))
            .send()?;
        Self::json(resp)
    }

    /// Patch an inventory item.
    pub fn update_inventory_item(
        &self,
        id: i64,
        input: &InventoryItemInput,
    ) -> Result<InventoryItem> {
        let resp = self
            .authed(
                self.http
                    .patch(self.url(&format!("/v1/maker/inventory_items/{id}"))),
            )?
            .json(&serde_json::json!({ "inventory_item": input }))
            .send()?;
        Self::json(resp)
    }

    /// Delete an inventory item.
    pub fn delete_inventory_item(&self, id: i64) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .delete(self.url(&format!("/v1/maker/inventory_items/{id}"))),
            )?
            .send()?;
        Self::ok_no_content(resp)
    }

    /// Adjust stock by `delta` (+/-). Returns the item with its new `quantity_cached`.
    pub fn adjust_inventory(
        &self,
        id: i64,
        delta: i64,
        reason: Option<&str>,
        note: Option<&str>,
    ) -> Result<InventoryItem> {
        let mut body = serde_json::json!({ "delta": delta });
        if let Some(r) = reason {
            body["reason"] = serde_json::json!(r);
        }
        if let Some(n) = note {
            body["note"] = serde_json::json!(n);
        }
        let resp = self
            .authed(self.http.post(self.url(&format!(
                "/v1/maker/inventory_items/{id}/adjust"
            ))))?
            .json(&body)
            .send()?;
        Self::json(resp)
    }

    /// An item's stock ledger (newest first).
    pub fn inventory_transactions(&self, id: i64) -> Result<Vec<InventoryTransaction>> {
        let resp = self
            .authed(self.http.get(self.url(&format!(
                "/v1/maker/inventory_items/{id}/transactions"
            ))))?
            .send()?;
        let r: InventoryTransactionsResponse = Self::json(resp)?;
        Ok(r.transactions)
    }

    // -- locations & suppliers --

    pub fn list_locations(&self) -> Result<Vec<Location>> {
        let resp = self
            .authed(self.http.get(self.url("/v1/maker/locations")))?
            .send()?;
        let r: LocationsResponse = Self::json(resp)?;
        Ok(r.locations)
    }

    pub fn create_location(
        &self,
        name: &str,
        description: Option<&str>,
        parent_id: Option<i64>,
    ) -> Result<Location> {
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/locations")))?
            .json(&serde_json::json!({ "location": {
                "name": name, "description": description, "parent_id": parent_id
            }}))
            .send()?;
        Self::json(resp)
    }

    pub fn delete_location(&self, id: i64) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .delete(self.url(&format!("/v1/maker/locations/{id}"))),
            )?
            .send()?;
        Self::ok_no_content(resp)
    }

    pub fn list_suppliers(&self) -> Result<Vec<Supplier>> {
        let resp = self
            .authed(self.http.get(self.url("/v1/maker/suppliers")))?
            .send()?;
        let r: SuppliersResponse = Self::json(resp)?;
        Ok(r.suppliers)
    }

    pub fn create_supplier(
        &self,
        name: &str,
        url: Option<&str>,
        notes: Option<&str>,
    ) -> Result<Supplier> {
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/suppliers")))?
            .json(&serde_json::json!({ "supplier": {
                "name": name, "url": url, "notes": notes
            }}))
            .send()?;
        Self::json(resp)
    }

    pub fn delete_supplier(&self, id: i64) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .delete(self.url(&format!("/v1/maker/suppliers/{id}"))),
            )?
            .send()?;
        Self::ok_no_content(resp)
    }

    // -- BOM --

    /// All BOMs (revisions) for a project, in revision order.
    pub fn list_boms(&self, project_id: i64) -> Result<Vec<Bom>> {
        let resp = self
            .authed(self.http.get(self.url(&format!(
                "/v1/maker/projects/{project_id}/boms"
            ))))?
            .send()?;
        let r: BomsResponse = Self::json(resp)?;
        Ok(r.boms)
    }

    /// Create a BOM (revision) under a project.
    pub fn create_bom(&self, project_id: i64, input: &BomInput) -> Result<Bom> {
        let resp = self
            .authed(self.http.post(self.url(&format!(
                "/v1/maker/projects/{project_id}/boms"
            ))))?
            .json(&serde_json::json!({ "bom": input }))
            .send()?;
        Self::json(resp)
    }

    /// One BOM's full view (includes `line_count`).
    pub fn get_bom(&self, id: i64) -> Result<Bom> {
        let resp = self
            .authed(self.http.get(self.url(&format!("/v1/maker/boms/{id}"))))?
            .send()?;
        Self::json(resp)
    }

    /// Delete a BOM.
    pub fn delete_bom(&self, id: i64) -> Result<()> {
        let resp = self
            .authed(self.http.delete(self.url(&format!("/v1/maker/boms/{id}"))))?
            .send()?;
        Self::ok_no_content(resp)
    }

    /// The roll-up: every line with its resolved target.
    pub fn bom_rollup(&self, id: i64) -> Result<BomRollup> {
        let resp = self
            .authed(
                self.http
                    .get(self.url(&format!("/v1/maker/boms/{id}/rollup"))),
            )?
            .send()?;
        Self::json(resp)
    }

    /// All lines of a BOM.
    pub fn list_bom_lines(&self, bom_id: i64) -> Result<Vec<BomLine>> {
        let resp = self
            .authed(self.http.get(self.url(&format!(
                "/v1/maker/boms/{bom_id}/bom_lines"
            ))))?
            .send()?;
        let r: BomLinesResponse = Self::json(resp)?;
        Ok(r.bom_lines)
    }

    /// Add a line to a BOM (set exactly one target in `input`).
    pub fn create_bom_line(&self, bom_id: i64, input: &BomLineInput) -> Result<BomLine> {
        let resp = self
            .authed(self.http.post(self.url(&format!(
                "/v1/maker/boms/{bom_id}/bom_lines"
            ))))?
            .json(&serde_json::json!({ "bom_line": input }))
            .send()?;
        Self::json(resp)
    }

    /// Patch a BOM line (shallow route — keyed by the line id).
    pub fn update_bom_line(&self, line_id: i64, input: &BomLineInput) -> Result<BomLine> {
        let resp = self
            .authed(
                self.http
                    .patch(self.url(&format!("/v1/maker/bom_lines/{line_id}"))),
            )?
            .json(&serde_json::json!({ "bom_line": input }))
            .send()?;
        Self::json(resp)
    }

    /// Remove a BOM line.
    pub fn delete_bom_line(&self, line_id: i64) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .delete(self.url(&format!("/v1/maker/bom_lines/{line_id}"))),
            )?
            .send()?;
        Self::ok_no_content(resp)
    }

    // -- previews --

    /// List previews, optionally filtered by node/version/kind.
    pub fn list_previews(
        &self,
        node_id: Option<&str>,
        version_id: Option<&str>,
        kind: Option<&str>,
    ) -> Result<Vec<Preview>> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(n) = node_id {
            query.push(("node_id", n.to_string()));
        }
        if let Some(v) = version_id {
            query.push(("version_id", v.to_string()));
        }
        if let Some(k) = kind {
            query.push(("kind", k.to_string()));
        }
        let resp = self
            .authed(self.http.get(self.url("/v1/maker/previews")))?
            .query(&query)
            .send()?;
        let r: PreviewsResponse = Self::json(resp)?;
        Ok(r.previews)
    }

    /// Enqueue (or fetch the existing) preview for a node/version/kind. Idempotent.
    pub fn enqueue_preview(
        &self,
        node_id: &str,
        version_id: Option<&str>,
        kind: Option<&str>,
        filename: Option<&str>,
    ) -> Result<Preview> {
        let input = PreviewInput {
            node_id,
            version_id,
            kind,
            filename,
        };
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/previews")))?
            .json(&serde_json::json!({ "preview": input }))
            .send()?;
        Self::json(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_short_shape_deserializes() {
        // The exact JSON the items controller renders for `index` (short form).
        let body = r#"{"id":7,"title":"Benchy","summary":null,"item_type":"model_3d",
            "origin":"downloaded","source_url":"https://printables.com/benchy",
            "source_name":null,"primary_node_id":"node-1","favorited":false,
            "created_at":"2026-06-15T00:00:00Z","updated_at":"2026-06-15T00:00:00Z"}"#;
        let item: Item = serde_json::from_str(body).unwrap();
        assert_eq!(item.id, 7);
        assert_eq!(item.item_type, "model_3d");
        assert_eq!(item.primary_node_id.as_deref(), Some("node-1"));
        assert!(!item.favorited);
    }

    #[test]
    fn item_detail_full_shape_deserializes() {
        // The full view: short fields + description/instructions/file_attachments/tags.
        let body = r##"{"id":1,"title":"Lamp","summary":"a lamp","item_type":"cad",
            "origin":"self_made","source_url":null,"source_name":null,
            "primary_node_id":null,"favorited":true,
            "created_at":"x","updated_at":"y","description":"desc","instructions":"# steps",
            "file_attachments":[
              {"id":3,"node_id":"n-9","version_id":"v-2","role":"model","label":"body.step","position":0}
            ],
            "tags":["printed","useful"],
            "taggings":[
              {"id":11,"tag_id":1,"name":"printed"},
              {"id":12,"tag_id":2,"name":"useful"}
            ]}"##;
        let d: ItemDetail = serde_json::from_str(body).unwrap();
        assert_eq!(d.title, "Lamp");
        assert!(d.favorited);
        assert_eq!(d.instructions.as_deref(), Some("# steps"));
        assert_eq!(d.file_attachments.len(), 1);
        assert_eq!(d.file_attachments[0].node_id, "n-9");
        assert_eq!(d.file_attachments[0].role, "model");
        assert_eq!(d.tags, vec!["printed".to_string(), "useful".to_string()]);
        // The taggings carry the join-row ids the in-app untag button needs.
        assert_eq!(d.taggings.len(), 2);
        assert_eq!(d.taggings[0].id, 11);
        assert_eq!(d.taggings[0].tag_id, 1);
        assert_eq!(d.taggings[0].name, "printed");
        assert_eq!(d.taggings[1].id, 12);
        assert_eq!(d.taggings[1].name, "useful");
    }

    #[test]
    fn item_detail_taggings_default_when_absent() {
        // An older/short item body without `taggings` must still parse (serde default = []).
        let body = r#"{"id":3,"title":"Old","item_type":"other","origin":"downloaded",
            "favorited":false,"tags":["a"]}"#;
        let d: ItemDetail = serde_json::from_str(body).unwrap();
        assert!(d.taggings.is_empty());
        assert_eq!(d.tags, vec!["a".to_string()]);
    }

    #[test]
    fn input_structs_deserialize_as_tauri_args() {
        // These arrive FROM the JS frontend as Tauri command args, so they must
        // `Deserialize` (the camelCase↔snake_case is handled by Tauri; serde sees snake_case).
        let item: ItemInput =
            serde_json::from_str(r#"{"title":"Vase","item_type":"model_3d","favorited":true}"#)
                .unwrap();
        assert_eq!(item.title.as_deref(), Some("Vase"));
        assert_eq!(item.item_type.as_deref(), Some("model_3d"));
        assert_eq!(item.favorited, Some(true));

        let att: FileAttachmentInput =
            serde_json::from_str(r#"{"node_id":"n-1","role":"model","label":"body.step"}"#)
                .unwrap();
        assert_eq!(att.node_id, "n-1");
        assert_eq!(att.role.as_deref(), Some("model"));

        let task: TaskInput = serde_json::from_str(r#"{"status":"doing"}"#).unwrap();
        assert_eq!(task.status.as_deref(), Some("doing"));
        assert_eq!(task.title, None);
    }

    #[test]
    fn item_detail_tolerates_absent_full_fields() {
        // A freshly created empty item: serde `default` must fill attachments/tags as empty.
        let body = r#"{"id":2,"title":"Bare","item_type":"other","origin":"downloaded",
            "favorited":false}"#;
        let d: ItemDetail = serde_json::from_str(body).unwrap();
        assert_eq!(d.title, "Bare");
        assert!(d.file_attachments.is_empty());
        assert!(d.tags.is_empty());
        assert_eq!(d.description, None);
    }

    #[test]
    fn item_input_omits_none_fields() {
        // A partial update sends only the keys it sets — Rails leaves the rest untouched.
        let input = ItemInput {
            favorited: Some(true),
            ..Default::default()
        };
        let v = serde_json::to_value(&input).unwrap();
        assert_eq!(v, serde_json::json!({ "favorited": true }));

        // `new(title)` sets exactly one field.
        let v2 = serde_json::to_value(ItemInput::new("Benchy")).unwrap();
        assert_eq!(v2, serde_json::json!({ "title": "Benchy" }));
    }

    #[test]
    fn item_input_full_round_trips_through_the_item_envelope() {
        // The shape `create_item`/`update_item` actually send: `{ "item": { … } }`.
        let input = ItemInput {
            title: Some("Vase".into()),
            item_type: Some("model_3d".into()),
            origin: Some("self_made".into()),
            instructions: Some("# print at 0.2mm".into()),
            ..Default::default()
        };
        let body = serde_json::json!({ "item": &input });
        assert_eq!(
            body,
            serde_json::json!({ "item": {
                "title": "Vase", "item_type": "model_3d", "origin": "self_made",
                "instructions": "# print at 0.2mm"
            }})
        );
    }

    #[test]
    fn file_attachment_input_omits_optional_fields() {
        let input = FileAttachmentInput {
            node_id: "node-abc".into(),
            role: Some("model".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&input).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "node_id": "node-abc", "role": "model" })
        );
    }

    #[test]
    fn file_attachment_response_deserializes() {
        let body = r#"{"id":5,"node_id":"n-1","version_id":null,"role":"image","label":null,"position":2}"#;
        let att: FileAttachment = serde_json::from_str(body).unwrap();
        assert_eq!(att.id, 5);
        assert_eq!(att.role, "image");
        assert_eq!(att.position, 2);
        assert_eq!(att.version_id, None);
    }

    #[test]
    fn tags_and_tagging_shapes_deserialize() {
        let tags: TagsResponse =
            serde_json::from_str(r##"{"tags":[{"id":1,"name":"printed","color":"#f00"},{"id":2,"name":"useful","color":null}]}"##)
                .unwrap();
        assert_eq!(tags.tags.len(), 2);
        assert_eq!(tags.tags[0].name, "printed");
        assert_eq!(tags.tags[1].color, None);

        let t: TaggingResponse = serde_json::from_str(r#"{"tag":"printed"}"#).unwrap();
        assert_eq!(t.tag, "printed");
    }

    #[test]
    fn collection_shapes_deserialize() {
        let cols: CollectionsResponse = serde_json::from_str(
            r#"{"collections":[{"id":1,"name":"Lamp parts","description":"v2","item_count":3,"updated_at":"x"}]}"#,
        )
        .unwrap();
        assert_eq!(cols.collections[0].name, "Lamp parts");
        assert_eq!(cols.collections[0].item_count, 3);

        let items: CollectionItemsResponse =
            serde_json::from_str(r#"{"items":[{"id":9,"title":"Knob","item_type":"cad"}]}"#)
                .unwrap();
        assert_eq!(items.items.len(), 1);
        assert_eq!(items.items[0].title, "Knob");
    }

    #[test]
    fn task_shapes_deserialize() {
        let body = r#"{"tasks":[
            {"id":1,"title":"Buy filament","notes":null,"status":"todo","position":0,
             "completed_at":null,"taskable_type":null,"taskable_id":null,
             "created_at":"x","updated_at":"y"},
            {"id":2,"title":"Print it","notes":"PLA","status":"done","position":1,
             "completed_at":"z","taskable_type":"Maker::Item","taskable_id":42,
             "created_at":"x","updated_at":"y"}
        ]}"#;
        let r: TasksResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.tasks.len(), 2);
        assert_eq!(r.tasks[0].status, "todo");
        assert_eq!(r.tasks[0].taskable_id, None);
        assert_eq!(r.tasks[1].status, "done");
        assert_eq!(r.tasks[1].taskable_type.as_deref(), Some("Maker::Item"));
        assert_eq!(r.tasks[1].taskable_id, Some(42));
    }

    #[test]
    fn task_input_attaches_to_an_item() {
        let input = TaskInput {
            title: Some("Print it".into()),
            taskable_type: Some("Maker::Item".into()),
            taskable_id: Some(42),
            ..Default::default()
        };
        let body = serde_json::json!({ "task": &input });
        assert_eq!(
            body,
            serde_json::json!({ "task": {
                "title": "Print it", "taskable_type": "Maker::Item", "taskable_id": 42
            }})
        );

        // A status-only patch (move on the board) sends just `status`.
        let patch = TaskInput {
            status: Some("doing".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&patch).unwrap(),
            serde_json::json!({ "status": "doing" })
        );
    }

    // ---- projects ----------------------------------------------------------------

    #[test]
    fn project_short_and_full_shapes_deserialize() {
        // index (short) shape from ProjectsController#project_json (full: false).
        let short: Project = serde_json::from_str(
            r#"{"id":4,"name":"CNC table","slug":"cnc-table","status":"active",
                "archived_at":null,"created_at":"x","updated_at":"y"}"#,
        )
        .unwrap();
        assert_eq!(short.id, 4);
        assert_eq!(short.status, "active");
        assert!(short.description.is_none());
        assert!(short.build_log_entries.is_empty());

        // show (full) shape: + description, build_log_entries, file_attachments.
        let full: Project = serde_json::from_str(
            r#"{"id":4,"name":"CNC table","slug":"cnc-table","status":"active",
                "archived_at":null,"created_at":"x","updated_at":"y",
                "description":"a big build",
                "build_log_entries":[
                  {"id":1,"body":"started","entry_type":"note","logged_at":"t"}
                ],
                "file_attachments":[
                  {"id":2,"node_id":"n-7","version_id":null,"role":"reference","label":null,"position":0}
                ]}"#,
        )
        .unwrap();
        assert_eq!(full.description.as_deref(), Some("a big build"));
        assert_eq!(full.build_log_entries.len(), 1);
        assert_eq!(full.build_log_entries[0].body, "started");
        assert_eq!(full.file_attachments.len(), 1);
        assert_eq!(full.file_attachments[0].node_id, "n-7");
    }

    #[test]
    fn project_input_omits_none_and_wraps() {
        let input = ProjectInput {
            name: Some("Lamp".into()),
            status: Some("planning".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&input).unwrap(),
            serde_json::json!({ "name": "Lamp", "status": "planning" })
        );
        assert_eq!(
            serde_json::to_value(ProjectInput::new("Lamp")).unwrap(),
            serde_json::json!({ "name": "Lamp" })
        );
    }

    // ---- inventory ---------------------------------------------------------------

    #[test]
    fn inventory_item_shapes_deserialize() {
        // quantity_cached / min_quantity are Rails BigDecimals → JSON **strings** (the real
        // wire shape from the controller, verified by live curl). de_decimal parses them.
        let short: InventoryItem = serde_json::from_str(
            r#"{"id":9,"name":"M3 bolt","part_number":"M3x10","category":"fasteners",
                "unit":"each","quantity_cached":"120.0","min_quantity":"50.0",
                "location_id":2,"supplier_id":3,"updated_at":"y"}"#,
        )
        .unwrap();
        assert_eq!(short.name, "M3 bolt");
        assert_eq!(short.quantity_cached, 120.0);
        assert_eq!(short.min_quantity, 50.0);
        assert_eq!(short.location_id, Some(2));
        // full-only fields default when absent
        assert!(short.notes.is_none());

        // de_decimal also tolerates a plain JSON number (robustness).
        let full: InventoryItem = serde_json::from_str(
            r#"{"id":9,"name":"M3 bolt","part_number":null,"category":null,"unit":"each",
                "quantity_cached":10.5,"min_quantity":0.0,"location_id":null,"supplier_id":null,
                "updated_at":"y","unit_cost":"0.05","notes":"hex head","created_at":"x"}"#,
        )
        .unwrap();
        assert_eq!(full.quantity_cached, 10.5);
        assert_eq!(full.notes.as_deref(), Some("hex head"));
        assert_eq!(full.unit_cost.as_deref(), Some("0.05"));
    }

    #[test]
    fn preview_input_omits_none_fields_from_the_wire() {
        // ASSERTED ON THE SERIALIZED BYTES, NOT A ROUND-TRIP (CLAUDE.md §14e, the FakeS3
        // lesson): `serde_json::from_str` reads an absent key and an explicit `null` into the
        // SAME `None`, so any assertion that parses this back confirms the bug instead of
        // catching it. Only the string can tell the two apart, and only the string is what the
        // server sees.
        let body = serde_json::to_string(&PreviewInput {
            node_id: "n1.svg",
            ..Default::default()
        })
        .unwrap();
        assert_eq!(body, r#"{"node_id":"n1.svg"}"#);
        // Named individually so each `skip_serializing_if` carries its own weight — asserting
        // only `kind` is how the other two stayed unguarded through the inline-`json!` era.
        assert!(!body.contains("\"kind\""), "kind leaked: {body}");
        assert!(!body.contains("\"version_id\""), "version_id leaked: {body}");
        assert!(!body.contains("\"filename\""), "filename leaked: {body}");

        // A value that IS supplied still goes out, under its own key.
        let full = serde_json::to_string(&PreviewInput {
            node_id: "n1.svg",
            version_id: Some("v1"),
            kind: Some("thumbnail"),
            filename: Some("plate.stl"),
        })
        .unwrap();
        assert!(full.contains(r#""kind":"thumbnail""#), "{full}");
        assert!(full.contains(r#""version_id":"v1""#), "{full}");
        assert!(full.contains(r#""filename":"plate.stl""#), "{full}");
    }

    #[test]
    fn preview_input_rides_inside_the_preview_envelope() {
        // The shape `enqueue_preview` actually sends. The envelope is what the controller's
        // `params.require(:preview)` reads, so a body missing it is a 400 rather than a create.
        let body = serde_json::json!({ "preview": PreviewInput {
            node_id: "n1.svg",
            kind: Some("thumbnail"),
            ..Default::default()
        }});
        assert_eq!(
            body,
            serde_json::json!({ "preview": { "node_id": "n1.svg", "kind": "thumbnail" } })
        );
    }

    #[test]
    fn inventory_item_input_omits_none() {
        let input = InventoryItemInput {
            name: Some("Resin".into()),
            unit: Some("l".into()),
            min_quantity: Some(1.0),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&input).unwrap(),
            serde_json::json!({ "name": "Resin", "unit": "l", "min_quantity": 1.0 })
        );
    }

    #[test]
    fn inventory_transaction_shape_deserializes() {
        // delta is a Rails decimal → JSON string (real wire shape).
        let r: InventoryTransactionsResponse = serde_json::from_str(
            r#"{"transactions":[
              {"id":1,"delta":"-5.0","reason":"consumed","project_id":2,"note":"prototype","created_at":"t"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(r.transactions.len(), 1);
        assert_eq!(r.transactions[0].delta, -5.0);
        assert_eq!(r.transactions[0].project_id, Some(2));
    }

    #[test]
    fn location_and_supplier_shapes_deserialize() {
        let loc: Location = serde_json::from_str(
            r#"{"id":1,"name":"Shelf A","description":"by the door","parent_id":null,
                "created_at":"x","updated_at":"y"}"#,
        )
        .unwrap();
        assert_eq!(loc.name, "Shelf A");

        let sup: Supplier = serde_json::from_str(
            r#"{"id":3,"name":"McMaster","url":"https://mcmaster.com","notes":null,
                "created_at":"x","updated_at":"y"}"#,
        )
        .unwrap();
        assert_eq!(sup.name, "McMaster");
        assert_eq!(sup.url.as_deref(), Some("https://mcmaster.com"));
    }

    // ---- BOM ---------------------------------------------------------------------

    #[test]
    fn bom_shapes_deserialize() {
        let r: BomsResponse = serde_json::from_str(
            r#"{"boms":[{"id":1,"project_id":4,"name":"Main","revision":1,
                "created_at":"x","updated_at":"y"}]}"#,
        )
        .unwrap();
        assert_eq!(r.boms[0].revision, 1);
        assert_eq!(r.boms[0].project_id, 4);

        let full: Bom = serde_json::from_str(
            r#"{"id":1,"project_id":4,"name":"Main","revision":2,
                "created_at":"x","updated_at":"y","line_count":3}"#,
        )
        .unwrap();
        assert_eq!(full.line_count, Some(3));
    }

    #[test]
    fn bom_rollup_three_target_kinds_deserialize() {
        // Mirrors BomsController#rollup → resolved_target's three shapes.
        let r: BomRollup = serde_json::from_str(
            r#"{
              "bom":{"id":1,"project_id":4,"name":"Main","revision":1,
                     "created_at":"x","updated_at":"y"},
              "lines":[
                {"id":10,"reference":"R1","quantity":"4.0","unit":"each","notes":null,
                 "position":0,"target_kind":"inventory_item",
                 "target":{"kind":"inventory_item","inventory_item":{"id":9,"name":"M3 bolt",
                   "part_number":"M3x10","unit":"each","quantity_cached":"120.0"}}},
                {"id":11,"reference":"R2","quantity":"1.0","unit":"each","notes":"printed body",
                 "position":1,"target_kind":"design_file",
                 "target":{"kind":"design_file","node_id":"node-abc"}},
                {"id":12,"reference":"R3","quantity":"2.0","unit":"m","notes":null,
                 "position":2,"target_kind":"free_text",
                 "target":{"kind":"free_text","part_name":"wire","supplier_id":3,
                   "supplier_name":"McMaster"}}
              ]}"#,
        )
        .unwrap();
        assert_eq!(r.bom.revision, 1);
        assert_eq!(r.lines.len(), 3);
        assert_eq!(r.lines[0].quantity, 4.0);
        assert_eq!(r.lines[0].target_kind.as_deref(), Some("inventory_item"));
        // target is free-form JSON the UI renders per kind.
        assert_eq!(r.lines[0].target["kind"], "inventory_item");
        assert_eq!(r.lines[0].target["inventory_item"]["name"], "M3 bolt");
        assert_eq!(r.lines[1].target["node_id"], "node-abc");
        assert_eq!(r.lines[2].target["part_name"], "wire");
        assert_eq!(r.lines[2].target["supplier_name"], "McMaster");
    }

    #[test]
    fn bom_line_input_links_exactly_one_target() {
        // design-file line
        let df = BomLineInput {
            quantity: Some(1.0),
            unit: Some("each".into()),
            node_id: Some("node-abc".into()),
            reference: Some("R2".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&df).unwrap(),
            serde_json::json!({ "quantity": 1.0, "unit": "each",
                "node_id": "node-abc", "reference": "R2" })
        );

        // inventory line
        let inv = BomLineInput {
            quantity: Some(4.0),
            unit: Some("each".into()),
            inventory_item_id: Some(9),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&inv).unwrap(),
            serde_json::json!({ "quantity": 4.0, "unit": "each", "inventory_item_id": 9 })
        );
    }

    #[test]
    fn bom_line_response_deserializes() {
        let line: BomLine = serde_json::from_str(
            r#"{"id":10,"bom_id":1,"reference":"R1","quantity":"4.0","unit":"each",
                "notes":null,"position":0,"inventory_item_id":9,"node_id":null,
                "part_name":null,"supplier_id":null,"target_kind":"inventory_item",
                "created_at":"x","updated_at":"y"}"#,
        )
        .unwrap();
        assert_eq!(line.id, 10);
        assert_eq!(line.quantity, 4.0);
        assert_eq!(line.inventory_item_id, Some(9));
        assert_eq!(line.target_kind.as_deref(), Some("inventory_item"));
    }

    // ---- previews ----------------------------------------------------------------

    #[test]
    fn preview_shape_deserializes() {
        let r: PreviewsResponse = serde_json::from_str(
            r#"{"previews":[
              {"id":1,"node_id":"n-1","version_id":"v-1","kind":"thumbnail","status":"ready",
               "error":null,"width":512,"height":512,"url":"https://blob/x.png",
               "created_at":"x","updated_at":"y"},
              {"id":2,"node_id":"n-2","version_id":null,"kind":"thumbnail","status":"pending",
               "error":null,"width":null,"height":null,"url":null,
               "created_at":"x","updated_at":"y"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(r.previews.len(), 2);
        assert_eq!(r.previews[0].status, "ready");
        assert_eq!(r.previews[0].url.as_deref(), Some("https://blob/x.png"));
        assert_eq!(r.previews[0].width, Some(512));
        assert_eq!(r.previews[1].status, "pending");
        assert!(r.previews[1].url.is_none());
    }
}
