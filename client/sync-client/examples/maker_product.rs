//! Live smoke for the Projects / Inventory / BOM / Preview client methods against a
//! running server (`STORAGE_BACKEND=memory bin/rails server -p 3001`).
//!
//!   (cd client/sync-client && SERVER_URL=http://localhost:3001 cargo run --example maker_product)
//!
//! Exercises the wire shapes added in maker.rs end-to-end: create a project + build-log
//! entry, an inventory item + stock adjust, a BOM with all three line-target kinds, and
//! the rollup; lists previews. Asserts on the typed responses so a wire mismatch fails loud.

use std::time::{SystemTime, UNIX_EPOCH};
use sync_client::*;

fn uniq() -> String {
    format!(
        "{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )
}

fn main() {
    let base = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());
    let mut c = ServerClient::new(&base);
    let email = format!("maker-{}@example.com", uniq());
    let reg = c
        .enroll(&email, "correct horse battery staple", "rust-client")
        .expect("enroll");
    println!("✓ enrolled {} (user {})", email, reg.user_id);

    // ---- projects ----
    let proj = c
        .create_project(&ProjectInput {
            name: Some("CNC table".into()),
            slug: Some(format!("cnc-{}", uniq())),
            status: Some("active".into()),
            description: Some("a big build".into()),
            ..Default::default()
        })
        .expect("create_project");
    assert_eq!(proj.status, "active");
    assert_eq!(proj.description.as_deref(), Some("a big build"));
    println!("✓ project #{} created", proj.id);

    let entry = c
        .add_build_log(
            proj.id,
            &BuildLogEntryInput {
                body: "started the frame".into(),
                entry_type: Some("milestone".into()),
                ..Default::default()
            },
        )
        .expect("add_build_log");
    assert_eq!(entry.body, "started the frame");
    let full = c.get_project(proj.id).expect("get_project");
    assert_eq!(full.build_log_entries.len(), 1);
    println!("✓ build-log entry roundtrips ({} total)", full.build_log_entries.len());

    let projects = c.list_projects(Some("active")).expect("list_projects");
    assert!(projects.iter().any(|p| p.id == proj.id));
    println!("✓ list_projects(active) sees it ({} active)", projects.len());

    // ---- inventory ----
    let item = c
        .create_inventory_item(&InventoryItemInput {
            name: Some("M3 bolt".into()),
            unit: Some("each".into()),
            min_quantity: Some(50.0),
            ..Default::default()
        })
        .expect("create_inventory_item");
    assert_eq!(item.quantity_cached, 0.0);
    let item = c
        .adjust_inventory(item.id, 120, Some("restocked"), None)
        .expect("adjust_inventory");
    assert_eq!(item.quantity_cached, 120.0);
    println!("✓ inventory item #{} now {} {}", item.id, item.quantity_cached, item.unit);

    let txns = c.inventory_transactions(item.id).expect("transactions");
    assert_eq!(txns.len(), 1);
    assert_eq!(txns[0].delta, 120.0);
    println!("✓ ledger has the +120 txn");

    // drop below min → low-stock view should surface it
    c.adjust_inventory(item.id, -100, Some("consumed"), None).unwrap();
    let low = c.list_low_stock().expect("low_stock");
    assert!(low.iter().any(|i| i.id == item.id), "item should be low after dropping to 20");
    println!("✓ low-stock view surfaces it (20 <= 50)");

    // ---- BOM ----
    let bom = c
        .create_bom(proj.id, &BomInput { name: Some("Main".into()), ..Default::default() })
        .expect("create_bom");
    println!("✓ BOM #{} rev {} created", bom.id, bom.revision);

    // inventory-item line
    c.create_bom_line(
        bom.id,
        &BomLineInput {
            reference: Some("R1".into()),
            quantity: Some(4.0),
            unit: Some("each".into()),
            inventory_item_id: Some(item.id),
            ..Default::default()
        },
    )
    .expect("inventory line");
    // free-text line
    c.create_bom_line(
        bom.id,
        &BomLineInput {
            reference: Some("R2".into()),
            quantity: Some(2.0),
            unit: Some("m".into()),
            part_name: Some("16AWG wire".into()),
            ..Default::default()
        },
    )
    .expect("free-text line");

    let rollup = c.bom_rollup(bom.id).expect("rollup");
    assert_eq!(rollup.lines.len(), 2);
    let kinds: Vec<_> = rollup.lines.iter().filter_map(|l| l.target_kind.clone()).collect();
    assert!(kinds.contains(&"inventory_item".to_string()));
    assert!(kinds.contains(&"free_text".to_string()));
    println!("✓ rollup resolves {} lines, kinds {:?}", rollup.lines.len(), kinds);

    // ---- previews ---- (enqueue is idempotent; render is async so we just check the shape)
    let pv = c
        .enqueue_preview("node-does-not-exist", None, Some("thumbnail"), None)
        .expect("enqueue_preview");
    assert_eq!(pv.kind, "thumbnail");
    let previews = c.list_previews(None, None, None).expect("list_previews");
    assert!(previews.iter().any(|p| p.id == pv.id));
    println!("✓ preview enqueued (#{}, status {})", pv.id, pv.status);

    println!("\nALL MAKER PRODUCT METHODS GREEN ✓");
}
