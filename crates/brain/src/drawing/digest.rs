//! The current turn's `Drawings on screen` line: each of a thread's visible scenes, oldest
//! first, with its begin header and its objects' ids and kinds. Headers and objects come from
//! the drawing system at one moment, so they always agree.

use std::fmt;

use kumochi_protocol::message::ThreadId;
use serde_json::json;

use super::{Author, Drawing, Held};
use crate::context::PictureRecipient;

/// Bytes the listed `[id, kind]` pairs of every scene in one line take together, a comma each
/// (first value).
pub(crate) const DIGEST_CAP: usize = 8 * 1024;

/// One screen-free line and one line for each recipient of a visible screen scene.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Digest {
    #[cfg(test)]
    all: String,
    off_screen: String,
    screens: Vec<(PictureRecipient, String)>,
}

impl Default for Digest {
    fn default() -> Self {
        Self {
            #[cfg(test)]
            all: "[]".to_owned(),
            off_screen: "[]".to_owned(),
            screens: Vec::new(),
        }
    }
}

impl Digest {
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        &self.all
    }

    pub(crate) fn off_screen(&self) -> &str {
        &self.off_screen
    }

    pub(crate) fn line_for(&self, recipient: &PictureRecipient) -> Option<&str> {
        self.screens
            .iter()
            .find(|(owner, _)| owner == recipient)
            .map(|(_, line)| line.as_str())
    }

    pub(crate) fn recipients(&self) -> impl Iterator<Item = &PictureRecipient> {
        self.screens.iter().map(|(recipient, _)| recipient)
    }
}

#[cfg(test)]
impl Digest {
    /// A line as the drawing system renders one for a thread with no screen scene.
    pub(crate) fn of(line: &str) -> Self {
        Self {
            all: line.to_owned(),
            off_screen: line.to_owned(),
            screens: Vec::new(),
        }
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self
            .screens
            .iter()
            .map(|(_, line)| line.len())
            .fold(self.off_screen.len(), usize::max);
        write!(f, "Digest {{ bytes: {bytes} }}")
    }
}

impl Drawing {
    /// `thread`'s visible scenes, each with its objects in the order they were put. Scenes take
    /// room in [`DIGEST_CAP`] newest first, each from its newest object back to the first pair
    /// that doesn't fit, so an older scene may still list small pairs after a newer one ran out.
    /// A scene that lists fewer than it holds adds `"more"`, the count left out.
    pub(crate) fn digest(&self, thread: ThreadId) -> Digest {
        let public = |held: &Held| held.author == Author::Public;
        let mut screens = Vec::new();
        for recipient in self
            .scenes
            .iter()
            .filter(|held| held.thread == thread && !held.gone)
            .filter_map(|held| match &held.author {
                Author::Private(recipient) => Some(recipient),
                Author::Public | Author::Unknown => None,
            })
        {
            if !screens.iter().any(|(kept, _)| kept == recipient) {
                let line = self.line(thread, |held| {
                    public(held)
                        || matches!(&held.author, Author::Private(held) if held == recipient)
                });
                screens.push((recipient.clone(), line));
            }
        }
        Digest {
            #[cfg(test)]
            all: self.line(thread, |_| true),
            off_screen: self.line(thread, public),
            screens,
        }
    }

    pub(crate) fn visible_to(&self, thread: ThreadId, recipient: &PictureRecipient) -> bool {
        self.scenes.iter().any(|held| {
            held.thread == thread
                && !held.gone
                && matches!(&held.author, Author::Private(held) if held == recipient)
        })
    }

    /// One line of [`Self::digest`]: the scenes on a surface it keeps.
    fn line(&self, thread: ThreadId, keeps: impl Fn(&Held) -> bool) -> String {
        let mut room = DIGEST_CAP;
        let mut scenes = Vec::new();
        for held in self
            .scenes
            .iter()
            .rev()
            .filter(|held| held.thread == thread && !held.gone && keeps(held))
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
