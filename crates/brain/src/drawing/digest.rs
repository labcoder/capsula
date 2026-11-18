//! The current turn's `Drawings on screen` line: each of a thread's visible scenes, oldest
//! first, with its begin header and its objects' ids and kinds. Headers and objects come from
//! the drawing system at one moment, so they always agree. The `ChatGPT` connection gets the full
//! line regardless of the chosen model's image support. Every other connection gets the line
//! without screen scenes.

use std::fmt;

use kumochi_protocol::message::ThreadId;
use kumochi_scene::record::Surface;
use serde_json::json;

use super::Drawing;

/// Bytes the listed `[id, kind]` pairs of every scene in one line take together, a comma each
/// (first value).
pub(crate) const DIGEST_CAP: usize = 8 * 1024;

/// The rendered lines, each under its own cap: every visible scene, and every one but the
/// screen scenes. They hold headers the model wrote, so Debug shows only their lengths.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Digest {
    all: String,
    off_screen: String,
}

impl Default for Digest {
    fn default() -> Self {
        Self {
            all: "[]".to_owned(),
            off_screen: "[]".to_owned(),
        }
    }
}

impl Digest {
    /// The line with every scene, for `ChatGPT` regardless of the chosen model's image support.
    pub(crate) fn as_str(&self) -> &str {
        &self.all
    }

    /// The line without screen scenes, for every connection other than `ChatGPT`.
    pub(crate) fn off_screen(&self) -> &str {
        &self.off_screen
    }
}

#[cfg(test)]
impl Digest {
    /// A line as the drawing system renders one for a thread with no screen scene.
    pub(crate) fn of(line: &str) -> Self {
        Self {
            all: line.to_owned(),
            off_screen: line.to_owned(),
        }
    }
}

#[expect(
    clippy::missing_fields_in_debug,
    reason = "the line without screen scenes is part of the full one, so its length adds nothing"
)]
impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Digest")
            .field("bytes", &self.all.len())
            .finish()
    }
}

impl Drawing {
    /// `thread`'s visible scenes, each with its objects in the order they were put. Scenes take
    /// room in [`DIGEST_CAP`] newest first, each from its newest object back to the first pair
    /// that doesn't fit, so an older scene may still list small pairs after a newer one ran out.
    /// A scene that lists fewer than it holds adds `"more"`, the count left out.
    pub(crate) fn digest(&self, thread: ThreadId) -> Digest {
        Digest {
            all: self.line(thread, |_| true),
            off_screen: self.line(thread, |surface| surface != Surface::Screen),
        }
    }

    /// One line of [`Self::digest`]: the scenes on a surface it keeps.
    fn line(&self, thread: ThreadId, keeps: impl Fn(Surface) -> bool) -> String {
        let mut room = DIGEST_CAP;
        let mut scenes = Vec::new();
        for held in self.scenes.iter().rev().filter(|held| {
            held.thread == thread && !held.gone && keeps(held.state.header().surface)
        }) {
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
            let mut begin = json!(held.state.header());
            // The brain stamps a screen scene's display; the model never writes or reads it.
            if let Some(header) = begin.as_object_mut() {
                header.remove("display");
            }
            scenes.push(format!(
                "{{\"begin\":{begin},\"objects\":[{}]{more}}}",
                listed.join(",")
            ));
        }
        scenes.reverse();
        format!("[{}]", scenes.join(","))
    }
}
