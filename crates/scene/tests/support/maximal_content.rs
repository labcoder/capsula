//! Joint-cap inputs shared by fast validation and native-font residency measurements.

use kumochi_scene::{
    Record, Step,
    limits::{MAX_DRAWING_BYTES, MAX_RECORD_BYTES},
    parse_record,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Content {
    UnicodeBytes,
    AsciiCells,
}

#[expect(
    clippy::too_many_lines,
    reason = "keep bounded workload generation together"
)]
pub(crate) fn fixture(content: Content, scene: u8) -> (usize, Vec<Step>) {
    let ascii = matches!(content, Content::AsciiCells);
    let label = "🙂".repeat(347);
    let full_label = "🙂".repeat(400);
    let mut initial: Vec<_> = (0..4).map(|index| format!(r#"{{"table":{{"id":"t{index}","cols":["A","B","C","D","E","F","G","H"],"box":[0,0,16000,12000]}}}}"#)).collect();
    initial.extend((0..2).map(|index| format!(r#"{{"chart":{{"id":"c{index}","kind":"scatter","box":[0,0,4000,3000],"y":{{"min":0,"max":16}}}}}}"#)));
    initial.extend((40..125).map(|index| {
        format!(
            r#"{{"row":{{"id":"r{index}","in":"t{}","cells":[""]}}}}"#,
            index / 40
        )
    }));
    initial.extend((0..232).map(|index| {
        format!(
            r#"{{"shape":{{"id":"s{index}","kind":"polygon","points":[[0,0],[1,0],[1,1],[0,1]]}}}}"#
        )
    }));
    let mut records = vec![format!(
        r#"{{"id":"initial","put":[{}]}}"#,
        initial.join(",")
    )];
    for pair in 0..20 {
        let rows: Vec<_> = (pair * 2..pair * 2 + 2)
            .map(|index| {
                let text = if index < 2 { &full_label } else { &label };
                let cells = (0..8)
                    .map(|cell| {
                        let text = if ascii {
                            format!("~{index:03}{cell}{}", "W".repeat(395))
                        } else {
                            text.clone()
                        };
                        format!("\"{text}\"")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!(r#"{{"row":{{"id":"r{index}","in":"t0","cells":[{cells}]}}}}"#)
            })
            .collect();
        records.push(format!(
            r#"{{"id":"rows{pair}","put":[{}]}}"#,
            rows.join(",")
        ));
    }
    for pair in 0..6 {
        let series: Vec<_> = (pair * 2..pair * 2 + 2)
            .map(|index| {
                let count = [500, 500, 500, 500, 500, 500, 500, 1, 500, 500, 498, 1][index];
                let points = (0..count)
                    .map(|point| format!("[{point},{}]", point % 17))
                    .collect::<Vec<_>>()
                    .join(",");
                let chart = if index < 8 { "c0" } else { "c1" };
                format!(r#"{{"series":{{"id":"z{index}","in":"{chart}","data":[{points}]}}}}"#)
            })
            .collect();
        records.push(format!(
            r#"{{"id":"series{pair}","put":[{}]}}"#,
            series.join(",")
        ));
    }
    let changes_start = records.len();
    while records.len() < 64 {
        let index = records.len();
        let mut changes: Vec<_> = (0..4)
            .map(|table| {
                let color = if index % 2 == 0 { "red" } else { "blue" };
                format!(r#"{{"target":"t{table}","color":"{color}"}}"#)
            })
            .collect();
        // One point per chart changes the shared domain without a large source record.
        changes.extend([7, 11].map(|series| {
            format!(
                r#"{{"target":"z{series}","data":[[{},0]]}}"#,
                1000 + 1000 * (index % 2)
            )
        }));
        if ascii {
            changes.extend(
                (40 + (index - changes_start) * 3..(43 + (index - changes_start) * 3).min(125))
                    .map(|row| {
                        let cells = (0..8)
                            .map(|cell| format!("\"~{row:03}{cell}{}\"", "W".repeat(395)))
                            .collect::<Vec<_>>()
                            .join(",");
                        format!(r#"{{"target":"r{row}","cells":[{cells}]}}"#)
                    }),
            );
        }
        records.push(format!(
            r#"{{"id":"color{index}","set":[{}]}}"#,
            changes.join(",")
        ));
    }
    let encoded: Vec<_> = records
        .into_iter()
        .map(|record| format!(r#"{{"step":{record}}}"#))
        .collect();
    assert!(
        encoded
            .iter()
            .all(|record| record.len() <= MAX_RECORD_BYTES),
        "each native fixture record stays within the encoded record limit"
    );
    let header_bytes =
        br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"diagram"}}"#.len();
    let drawing_bytes =
        header_bytes + 1 + encoded.iter().map(|record| record.len() + 1).sum::<usize>();
    assert!(
        drawing_bytes <= MAX_DRAWING_BYTES,
        "{drawing_bytes} fixture bytes"
    );
    if !ascii {
        assert!(
            drawing_bytes > MAX_DRAWING_BYTES - 2048,
            "{drawing_bytes} fixture bytes"
        );
    }

    let steps = encoded
        .iter()
        .enumerate()
        .map(|(index, record)| {
            match parse_record(record.replace('~', &scene.to_string()).as_bytes()) {
                Ok(Record::Step(step)) => step,
                Ok(_) => panic!("fixture record {index} must be a step"),
                Err(error) => panic!("fixture record {index}: {error:?}"),
            }
        })
        .collect();
    (drawing_bytes, steps)
}
