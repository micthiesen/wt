use serde_json::{Value, json};

#[test]
fn work_status_matches_typescript_record_fixture() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../test/compat/work-status.json")).unwrap();
    let parsed = wt_core::parse_work_status(&fixture["input"]).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), fixture["expected"]);
}

#[test]
fn inferred_stack_matches_typescript_layout_fixture() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../test/compat/stack-layout.json")).unwrap();
    let members: Vec<wt_core::ChainMember> =
        serde_json::from_value(fixture["input"].clone()).unwrap();
    let stacks = wt_core::build_stack_index(&members, "main");
    let actual = stacks
        .layouts
        .iter()
        .map(|layout| {
            let nodes = layout
                .nodes
                .iter()
                .map(|node| {
                    let mut value = serde_json::to_value(node).unwrap();
                    // Rust also carries the owning stack id on each node. The
                    // reference fixture expresses that identity on the parent.
                    value.as_object_mut().unwrap().remove("stackId");
                    value
                })
                .collect::<Vec<_>>();
            json!({"stackId": layout.stack_id, "nodes": nodes})
        })
        .collect::<Vec<_>>();
    assert_eq!(json!(actual), fixture["expected"]);
}
