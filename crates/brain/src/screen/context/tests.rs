use serde::Serialize;
use zeroize::Zeroizing;

use super::ScreenContext;
use crate::screen::encode::{CaptureTransform, Encoded};
use crate::screen::mask::Completeness;
use crate::screen::wire::CaptureId;

const TRANSFORM: CaptureTransform = CaptureTransform {
    display: kumochi_protocol::geometry::PhysicalRect {
        origin: [0, 0],
        size: [4, 3],
    },
    dpi: 96,
    overview_size: [4, 3],
    crop: None,
};

fn context(jpeg: Vec<u8>) -> ScreenContext {
    ScreenContext::new(Encoded {
        jpeg: Zeroizing::new(jpeg),
        size: [4, 3],
        capture: CaptureId(1),
        generation: 1,
        completeness: Completeness::Complete,
        transform: TRANSFORM,
    })
}

/// The context as it was serialized before: its JPEG as a list of bytes.
#[derive(Serialize)]
struct Listed {
    jpeg: Vec<u8>,
    size: [u32; 2],
    transform: CaptureTransform,
}

#[test]
fn a_context_keeps_the_wire_bytes_of_a_list_of_bytes() {
    for len in [0_u32, 1, 127, 128, 300_000] {
        let jpeg: Vec<u8> = (0..len)
            .map(|at| u8::try_from(at % 251).expect("under 251"))
            .collect();
        let sent = postcard::to_allocvec(&context(jpeg.clone())).expect("encoded");
        let listed = Listed {
            jpeg,
            size: [4, 3],
            transform: TRANSFORM,
        };
        assert_eq!(
            sent,
            postcard::to_allocvec(&listed).expect("encoded"),
            "{len}"
        );
    }
}

#[test]
fn a_received_jpeg_is_one_allocation_of_its_exact_length() {
    let len = 2 * 1024 * 1024 - 3;
    let sent = postcard::to_allocvec(&context(vec![0x42; len])).expect("encoded");
    let received: ScreenContext = postcard::from_bytes(&sent).expect("decoded");
    assert_eq!(received.jpeg.len(), len);
    assert_eq!(received.jpeg.capacity(), len);
    assert_eq!((received.size, received.transform), ([4, 3], TRANSFORM));
    #[cfg(windows)]
    assert!(received.dump.is_some(), "registered for dump exclusion");
}
