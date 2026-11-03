use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(super) enum Fixture {
    Local,
    CrossGroup,
}

pub(super) fn capped_step(count: usize, fixture: Fixture) -> Value {
    assert!(
        [150, 600].contains(&count),
        "supported four-view object counts"
    );
    let cross = matches!(fixture, Fixture::CrossGroup);
    let mut put = Vec::with_capacity(count);
    for index in 0..14 {
        put.push(json!({"diagram":{
            "id":format!("g{index:x}"),
            "in":(index > 0).then(|| format!("g{:x}",index - 1)),
            "dir":match index { 0 => Some("down"), 13 => Some("right"), _ => None }
        }}));
    }
    let counts: &[usize] = if count == 600 {
        &[48, 16, 16, 16]
    } else {
        &[45]
    };
    let mut node = 0;
    let mut edge = 0;
    let mut starts = Vec::with_capacity(counts.len());
    for (group, &size) in counts.iter().enumerate() {
        let owner = format!("g{:x}", 13 + group);
        if group > 0 {
            put.push(json!({"diagram":{"id":owner,"in":"g0","dir":"right"}}));
        }
        starts.push(node);
        let first = node;
        for _ in 0..size {
            put.push(json!({"node":{"id":format!("{node:x}"),"in":owner,"label":"N"}}));
            node += 1;
        }
        let edges = if count == 150 {
            90
        } else if cross {
            72
        } else {
            96
        };
        for index in 0..edges {
            put.push(json!({"edge":{
                "id":format!("x{edge:x}"),"in":owner,
                "from":format!("{:x}",first + index % size),
                "to":format!("{:x}",first + (index * 5 + 1) % size)
            }}));
            edge += 1;
        }
    }
    if cross && count == 600 {
        for index in 0..96 {
            let from = index % counts.len();
            let to = (from + 1) % counts.len();
            put.push(json!({"edge":{
                "id":format!("x{edge:x}"),"in":"g0",
                "from":format!("{:x}",starts[from] + index % counts[from]),
                "to":format!("{:x}",starts[to] + (index * 5 + 1) % counts[to])
            }}));
            edge += 1;
        }
    }
    for index in 0..count - put.len() {
        put.push(json!({"line":{"id":format!("p{index:x}"),"from":[0,0],"to":[1,1]}}));
    }
    assert_eq!(put.len(), count, "fixture reaches its declared object cap");
    let value = json!({"id":"caps","put":put});
    let encoded = serde_json::to_vec(&json!({"step":value})).expect("fixture JSON");
    assert!(
        encoded.len() <= kumochi_scene::limits::MAX_RECORD_BYTES,
        "fixture fits the record byte cap"
    );
    value
}
