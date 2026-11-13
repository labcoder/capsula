//! The current turn's `Drawings on screen` line: each of a thread's visible scenes, oldest
//! first, with its begin header and its objects' ids and kinds. Headers and objects come from
//! the drawing system at one moment, so they always agree.

use std::fmt;

use kumochi_protocol::message::ThreadId;
use serde_json::json;

use super::Drawing;

/// Bytes the listed `[id, kind]` pairs of every scene take together, a comma each (first value).
pub(crate) const DIGEST_CAP: usize = 8 * 1024;

/// The rendered line. It holds headers the model wrote, so Debug shows only its length.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Digest(String);

impl Default for Digest {
    fn default() -> Self {
        Self("[]".to_owned())
    }
}

impl Digest {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
impl Digest {
    /// A line as the drawing system renders one.
    pub(crate) fn of(line: &str) -> Self {
        Self(line.to_owned())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Digest")
            .field("bytes", &self.0.len())
            .finish()
    }
}

impl Drawing {
    /// `thread`'s visible scenes, each with its objects in the order they were put. Scenes take
    /// room in [`DIGEST_CAP`] newest first, each from its newest object back to the first pair
    /// that doesn't fit, so an older scene may still list small pairs after a newer one ran out.
    /// A scene that lists fewer than it holds adds `"more"`, the count left out.
    pub(crate) fn digest(&self, thread: ThreadId) -> Digest {
        let mut room = DIGEST_CAP;
        let mut scenes = Vec::new();
        for held in self
            .scenes
            .iter()
            .rev()
            .filter(|held| held.thread == thread && !held.gone)
        {
            let mut objects: Vec<_> = held.state.objects().collect();
            objects.sort_unstable_by_key(|&(_, handle, _)| handle);
            let mut listed = Vec::new();
            for (id, _, kind) in objects.iter().rev() {
                let pair = json!([id, kind]).to_string();
                let Some(left) = room.checked_sub(pair.len() + 1) else {
                    break;
                };
                room = left;
                listed.push(pair);
            }
            let more = match objects.len() - listed.len() {
                0 => String::new(),
                more => format!(",\"more\":{more}"),
            };
            listed.reverse();
            let begin = json!(held.state.header());
            scenes.push(format!(
                "{{\"begin\":{begin},\"objects\":[{}]{more}}}",
                listed.join(",")
            ));
        }
        scenes.reverse();
        Digest(format!("[{}]", scenes.join(",")))
    }
}
