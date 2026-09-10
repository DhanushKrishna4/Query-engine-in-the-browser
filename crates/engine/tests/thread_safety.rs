//! The data an operator passes upward can cross a thread.
//!
//! The spec asks that the operator interface be structured so parallelism
//! could be added later without a rewrite. That is a claim about types, and a
//! claim about types is checkable: whatever an exchange operator would move
//! between threads has to be `Send`, and whatever two threads would read at
//! once has to be `Sync`.
//!
//! Nothing here runs in parallel today -- the wasm target is single-threaded,
//! because GitHub Pages cannot set the COOP/COEP headers `SharedArrayBuffer`
//! needs. This exists so that the day someone tries, the failure is a plan
//! change and not a storage-layer rewrite. An `Rc` slipped into a column would
//! break it silently otherwise, and would be found much later.

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn the_columnar_types_can_cross_threads() {
    // A batch is a pointer to each column plus a selection, so this is really
    // a claim about the columns underneath it.
    assert_send_sync::<engine::storage::Batch>();
    assert_send_sync::<engine::storage::Column>();
    assert_send_sync::<engine::storage::Bitmap>();

    // A table and its row groups are shared, not moved: two scan threads would
    // read the same `Arc<Table>`, which needs `Sync` rather than `Send`.
    assert_send_sync::<engine::storage::Table>();
    assert_send_sync::<engine::storage::RowGroup>();

    // Plans and expressions are built once and read by every operator.
    assert_send_sync::<engine::plan::LogicalPlan>();
    assert_send_sync::<engine::plan::BoundExpr>();
    assert_send_sync::<engine::storage::Schema>();

    // And the statistics an exchange would have to merge.
    assert_send_sync::<engine::exec::OperatorStats>();
}
