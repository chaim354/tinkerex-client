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

/// The full item view (`GET /v1/maker/items/:id`, and the body returned by
/// create/update): the short shape plus description/instructions/attachments/tags.
#[derive(Debug, Clone, Deserialize, PartialEq)]
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
}

/// The mutable fields of an item — used to build the `{ item: { … } }` body for
/// create/update. Every field is optional so `update_item` can send a partial patch;
/// `None` fields are omitted from the JSON (Rails leaves unpermitted/absent keys
/// untouched on update). `create_item` should at least set `title`.
#[derive(Debug, Clone, Default, Serialize)]
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
        Self { title: Some(title.into()), ..Default::default() }
    }
}

#[derive(Debug, Deserialize)]
pub struct ItemsResponse {
    pub items: Vec<Item>,
}

/// The body for attaching a file to an item (`POST /items/:id/file_attachments`).
#[derive(Debug, Clone, Default, Serialize)]
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Default, Serialize)]
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
        Self { title: Some(title.into()), ..Default::default() }
    }
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
            .authed(self.http.post(self.url(&format!("/v1/maker/items/{item_id}/file_attachments"))))?
            .json(&serde_json::json!({ "file_attachment": input }))
            .send()?;
        Self::json(resp)
    }

    /// Detach a file from its item (shallow route — keyed by the *attachment* id).
    pub fn detach_file(&self, attachment_id: i64) -> Result<()> {
        let resp = self
            .authed(self.http.delete(self.url(&format!("/v1/maker/file_attachments/{attachment_id}"))))?
            .send()?;
        Self::ok_no_content(resp)
    }

    // -- tags & taggings --

    /// Tag an item by tag *name* (the tag is created on the fly if new). Returns the
    /// normalized tag name the server stored.
    pub fn tag_item(&self, item_id: i64, name: &str) -> Result<String> {
        let resp = self
            .authed(self.http.post(self.url(&format!("/v1/maker/items/{item_id}/taggings"))))?
            .json(&serde_json::json!({ "name": name }))
            .send()?;
        let r: TaggingResponse = Self::json(resp)?;
        Ok(r.tag)
    }

    /// Remove a tagging (shallow route — keyed by the *tagging* id, not the tag id).
    pub fn untag(&self, tagging_id: i64) -> Result<()> {
        let resp = self
            .authed(self.http.delete(self.url(&format!("/v1/maker/taggings/{tagging_id}"))))?
            .send()?;
        Self::ok_no_content(resp)
    }

    /// All of the user's tags (for the tag filter chips / autocomplete).
    pub fn list_tags(&self) -> Result<Vec<Tag>> {
        let resp = self.authed(self.http.get(self.url("/v1/maker/tags")))?.send()?;
        let r: TagsResponse = Self::json(resp)?;
        Ok(r.tags)
    }

    // -- collections --

    /// All of the user's collections (name + item_count), alphabetically.
    pub fn list_collections(&self) -> Result<Vec<Collection>> {
        let resp = self.authed(self.http.get(self.url("/v1/maker/collections")))?.send()?;
        let r: CollectionsResponse = Self::json(resp)?;
        Ok(r.collections)
    }

    /// Create a collection.
    pub fn create_collection(&self, name: &str, description: Option<&str>) -> Result<Collection> {
        let resp = self
            .authed(self.http.post(self.url("/v1/maker/collections")))?
            .json(&serde_json::json!({ "collection": { "name": name, "description": description } }))
            .send()?;
        Self::json(resp)
    }

    /// The items in a collection (id/title/item_type each).
    pub fn collection_items(&self, collection_id: i64) -> Result<Vec<CollectionItem>> {
        let resp = self
            .authed(self.http.get(self.url(&format!("/v1/maker/collections/{collection_id}/items"))))?
            .send()?;
        let r: CollectionItemsResponse = Self::json(resp)?;
        Ok(r.items)
    }

    /// Add an item to a collection (idempotent server-side — `find_or_create_by`).
    pub fn add_to_collection(&self, collection_id: i64, item_id: i64) -> Result<()> {
        let resp = self
            .authed(self.http.post(self.url(&format!("/v1/maker/collections/{collection_id}/items"))))?
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
            "tags":["printed","useful"]}"##;
        let d: ItemDetail = serde_json::from_str(body).unwrap();
        assert_eq!(d.title, "Lamp");
        assert!(d.favorited);
        assert_eq!(d.instructions.as_deref(), Some("# steps"));
        assert_eq!(d.file_attachments.len(), 1);
        assert_eq!(d.file_attachments[0].node_id, "n-9");
        assert_eq!(d.file_attachments[0].role, "model");
        assert_eq!(d.tags, vec!["printed".to_string(), "useful".to_string()]);
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
        let input = ItemInput { favorited: Some(true), ..Default::default() };
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
        let input = FileAttachmentInput { node_id: "node-abc".into(), role: Some("model".into()), ..Default::default() };
        let v = serde_json::to_value(&input).unwrap();
        assert_eq!(v, serde_json::json!({ "node_id": "node-abc", "role": "model" }));
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
            serde_json::from_str(r#"{"items":[{"id":9,"title":"Knob","item_type":"cad"}]}"#).unwrap();
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
        let patch = TaskInput { status: Some("doing".into()), ..Default::default() };
        assert_eq!(serde_json::to_value(&patch).unwrap(), serde_json::json!({ "status": "doing" }));
    }
}
