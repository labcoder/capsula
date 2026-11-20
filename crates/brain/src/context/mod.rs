//! Builds each model request from the store (architecture v1 §6.5; plans/slice-3.md PR 12).
//!
//! A request is built from one read transaction: the run's thread, its newest summary and raw
//! runs, and every source its history results name, re-read through references so a deleted
//! task's text is never copied into another task. The run's origin decides what a reference may
//! reach. Nothing here sends anything: the page reader runs each build the coordinator asks for
//! (docs/development/engines.md).

pub(crate) mod bound;
pub(crate) mod build;
pub(crate) mod compact;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kumochi_protocol::message::{
    AssistantName, ConnectionKind, EventId, ModelChoice, RunId, ThreadId, UserName,
};
use zeroize::Zeroizing;

use crate::engines::{CallId, Opaque};
use crate::store::StoreError;

/// The request's hard size bound [guess]. With images, it bounds everything but their bytes.
pub(crate) const BOUND: usize = 512 * 1024;
/// The whole body's ceiling, images included [guess] (plans/slice-6.md C1, Captain call 4).
pub(crate) const BODY_CEILING: usize = 4 * 1024 * 1024;
/// How an image's bytes start in the body, before their base64.
const JPEG_DATA_URL: &str = "data:image/jpeg;base64,";
/// An image's Responses input item with an empty URL. The keys' order doesn't change its length.
const EMPTY_IMAGE_ITEM: &str = r#"{"type":"message","role":"user","content":[{"type":"input_image","image_url":"","detail":"high"}]}"#;
/// The newest runs kept raw, the current one included, when they fit [guess].
pub(crate) const RAW_RUNS: usize = 8;
/// The most one reference re-reads [guess].
pub(crate) const REREAD_CAP: usize = 32 * 1024;
/// What a reference to a deleted task renders as.
pub(crate) const REMOVED: &str = "[removed with a deleted task]";
/// What a reference the run's origin may not reach renders as.
pub(crate) const UNAVAILABLE: &str = "[not available here]";
/// The most deletion notes a request carries, newest kept [guess].
pub(crate) const MAX_NOTES: usize = 32;

/// Where a run was started, which decides what it may reach. It comes from the run's thread,
/// never from anything a model produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// Kumo's own conversation, which may reach every live task.
    Kumo,
    /// A task, which reaches only itself.
    Task(ThreadId),
}

/// Which connection produced a native item: the account's generation, fixed for a run at its
/// first build (D13-H1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionTag(pub(crate) u64);

/// What a run answers on: a connection kind and the sign-in it was bound to at its first build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) kind: ConnectionKind,
    pub(crate) tag: ConnectionTag,
}

#[cfg(test)]
impl Route {
    /// `ChatGPT` on the sign-in `tag`.
    pub(crate) fn chatgpt(tag: u64) -> Self {
        Self {
            kind: ConnectionKind::ChatGpt,
            tag: ConnectionTag(tag),
        }
    }
}

/// What a run reads once when it starts and reuses for every round, so a rename applies from the
/// next run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunSnapshot {
    pub(crate) origin: Origin,
    pub(crate) assistant: AssistantName,
    pub(crate) user: Option<UserName>,
    /// The thread's model, else the default; `None` until a catalog has been read. The fixture
    /// engine needs none, and a real engine (PR 13a) refuses a request without one.
    pub(crate) choice: Option<ModelChoice>,
    /// The tag the choice's kind had when the run's turn arrived, which its stored rounds carry;
    /// the store sets it as it reads the choice. `ConnectionTag(0)` when that kind had none: the
    /// run is then refused before anything is sent, so no replay it reads is ever sent.
    pub(crate) connection: ConnectionTag,
}

/// A range of a task's events that a history result showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRef {
    pub(crate) thread: ThreadId,
    pub(crate) first: EventId,
    pub(crate) last: EventId,
}

/// A summary of a thread's events up to and including `last`. PR 17 writes them.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) first: EventId,
    pub(crate) last: EventId,
    pub(crate) text: String,
}

impl fmt::Debug for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Summary")
            .field("first", &self.first)
            .field("last", &self.last)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

/// One item of a run, ordered by its event id.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "PR 14a stores history calls and their results")
)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Body {
    User(String),
    Assistant(String),
    /// An unspoken drawing line, not part of a provider's carried message.
    Narration {
        text: String,
        call: CallId,
    },
    /// A tool call; its result is the next [`Body::History`] with the same id.
    Call {
        call: CallId,
        name: String,
        arguments: String,
    },
    /// A history tool's result, stored as references only (PR 14a).
    History {
        call: CallId,
        refs: Vec<HistoryRef>,
    },
    /// A native output item of a stored round, replayed only on the connection that made it:
    /// its kind, `None` when the stored kind is one this build doesn't know, and its tag.
    Native {
        kind: Option<ConnectionKind>,
        connection: ConnectionTag,
        item: Opaque,
    },
    /// The result Kumochi answered a native call with, after its round's items.
    Result {
        call: CallId,
        output: String,
    },
    Recall {
        call: CallId,
        selection: crate::recall::Selection,
    },
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(text) => f.debug_tuple("User").field(&text.len()).finish(),
            Self::Assistant(text) => f.debug_tuple("Assistant").field(&text.len()).finish(),
            Self::Narration { text, .. } => f.debug_tuple("Narration").field(&text.len()).finish(),
            Self::Call { call, .. } => f.debug_struct("Call").field("call", call).finish(),
            Self::History { call, refs } => f
                .debug_struct("History")
                .field("call", call)
                .field("refs", refs)
                .finish(),
            Self::Native {
                kind,
                connection,
                item,
            } => f
                .debug_struct("Native")
                .field("kind", kind)
                .field("connection", connection)
                .field("item", item)
                .finish(),
            Self::Result { call, output } => f
                .debug_struct("Result")
                .field("call", call)
                .field("output_bytes", &output.len())
                .finish(),
            Self::Recall { call, selection } => f
                .debug_struct("Recall")
                .field("call", call)
                .field("selection", selection)
                .finish(),
        }
    }
}

/// A thread item: an event the store read, or one another part supplies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) id: EventId,
    pub(crate) run: RunId,
    pub(crate) body: Body,
}

/// What the parts that come later supply (PRs 13b, 14a and 17); until then, nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Supplied {
    pub(crate) summary: Option<Summary>,
    pub(crate) items: Vec<Item>,
    /// The thread's drawings on screen, taken by the drawing system when the build began.
    pub(crate) drawings: crate::drawing::Digest,
}

/// The input item a summary becomes.
pub(crate) fn summary_item(first: EventId, last: EventId, text: &str) -> InputItem {
    InputItem::Message {
        role: Role::Developer,
        text: format!("Summary of events {}–{}:\n{text}", first.0, last.0),
    }
}

/// The input item a deletion note becomes.
pub(crate) fn note_item(note: &crate::store::context::Note) -> InputItem {
    InputItem::Message {
        role: Role::Developer,
        text: format!("Note: a task was deleted at {}.", note.when),
    }
}

/// Who a message is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Developer,
    User,
    Assistant,
}

/// A sealed JPEG for the model. Its bytes are user content: Debug shows only their length, and
/// they're zeroed when the last copy drops.
#[derive(Clone)]
pub(crate) struct Jpeg(Arc<Image>);

/// The one buffer behind every copy of a [`Jpeg`].
struct Image {
    /// Declared first so it drops first: the registration ends after `Drop` zeroes the bytes and
    /// before they're freed.
    #[cfg(windows)]
    _dump: Option<crate::platform::DumpExclusion>,
    bytes: Zeroizing<Vec<u8>>,
}

impl Drop for Image {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut *self.bytes);
    }
}

impl PartialEq for Jpeg {
    fn eq(&self, other: &Self) -> bool {
        self.0.bytes == other.0.bytes
    }
}

impl Eq for Jpeg {}

impl Jpeg {
    /// A sealed image's own buffer, moved in without a copy, with the dump registration that
    /// already covers it.
    pub(crate) fn sealed(
        bytes: Zeroizing<Vec<u8>>,
        #[cfg(windows)] dump: Option<crate::platform::DumpExclusion>,
    ) -> Self {
        Self(Arc::new(Image {
            #[cfg(windows)]
            _dump: dump,
            bytes,
        }))
    }

    /// `bytes` when they start with a JPEG's SOI marker, else nothing; refused bytes are zeroed.
    #[cfg_attr(
        not(any(test, debug_assertions)),
        expect(dead_code, reason = "2c builds it from a hold's sealed screen image")
    )]
    pub(crate) fn new(bytes: Vec<u8>) -> Option<Self> {
        let bytes = Zeroizing::new(bytes);
        bytes.starts_with(&[0xFF, 0xD8, 0xFF]).then(|| {
            Self(Arc::new(Image {
                #[cfg(windows)]
                _dump: None,
                bytes,
            }))
        })
    }

    /// The image as the data URL the body carries, built in one zeroing buffer of exactly its
    /// length, so no copy is left behind.
    pub(crate) fn data_url(&self) -> Zeroizing<String> {
        let mut url = Zeroizing::new(String::with_capacity(self.data_url_len()));
        url.push_str(JPEG_DATA_URL);
        STANDARD.encode_string(self.0.bytes.as_slice(), &mut url);
        url
    }

    /// [`Self::data_url`]'s length, without building it.
    pub(crate) fn data_url_len(&self) -> usize {
        JPEG_DATA_URL.len() + self.0.bytes.len().div_ceil(3) * 4
    }

    /// The image's whole input item serialized, wrapper included, without building it. Exact,
    /// since neither the prefix nor base64 needs escaping.
    pub(crate) fn item_len(&self) -> usize {
        EMPTY_IMAGE_ITEM.len() + self.data_url_len()
    }
}

impl fmt::Debug for Jpeg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Jpeg").field(&self.0.bytes.len()).finish()
    }
}

/// One input item of the request.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum InputItem {
    Message {
        role: Role,
        text: String,
    },
    FunctionCall {
        call: CallId,
        name: String,
        arguments: String,
    },
    /// A tool result. A history result's output is quoted data, never instructions.
    FunctionOutput {
        call: CallId,
        output: String,
    },
    /// A provider's own output item, sent back as the same JSON value.
    Native(Opaque),
    /// An image from the user, right after the message it belongs to.
    Image(Jpeg),
}

impl fmt::Debug for InputItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message { role, text } => f
                .debug_struct("Message")
                .field("role", role)
                .field("text_bytes", &text.len())
                .finish(),
            Self::FunctionCall { call, .. } => {
                f.debug_struct("FunctionCall").field("call", call).finish()
            }
            Self::FunctionOutput { call, output } => f
                .debug_struct("FunctionOutput")
                .field("call", call)
                .field("output_bytes", &output.len())
                .finish(),
            Self::Native(item) => f.debug_tuple("Native").field(item).finish(),
            Self::Image(image) => f.debug_tuple("Image").field(image).finish(),
        }
    }
}

/// A tool the request offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolSchema {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) parameters: serde_json::Value,
}

/// The tools a run with `origin` is offered.
pub(crate) fn tools(origin: Origin) -> Vec<ToolSchema> {
    crate::dispatch::model_schemas(origin)
}

/// A built request, as data: PR 13a serialises it, with `store:false` and `stream:true`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ModelRequest {
    pub(crate) choice: Option<ModelChoice>,
    pub(crate) instructions: String,
    pub(crate) input: Vec<InputItem>,
    pub(crate) tools: Vec<ToolSchema>,
}

impl fmt::Debug for ModelRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRequest")
            .field("choice", &self.choice)
            .field("instructions_bytes", &self.instructions.len())
            .field("input", &self.input)
            .field("tools", &self.tools.len())
            .finish()
    }
}

/// Why an image wasn't attached; either way the request is sent without it, never silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ImageRefused {
    /// The route can't carry images: no model, or a connection other than `ChatGPT`'s.
    #[error("this route can't carry images")]
    Route,
    /// No user message to attach it to.
    #[error("no user message to attach the image to")]
    NoTurn,
    /// The screen's developer line isn't in the bundled messages.
    #[error("the screen's line isn't bundled")]
    Unbundled,
    /// The user turned screen holds off while the run held its image.
    #[error("the screen was withdrawn")]
    Withdrawn,
}

impl ModelRequest {
    /// Adds `image` right after the newest user message, for a live check's own `ChatGPT`
    /// request: any other route refuses here. The body's ceiling is checked when it's serialised.
    pub(crate) fn attach(&mut self, image: Jpeg) -> Result<(), ImageRefused> {
        self.insert_image(image, takes_images(self.choice.as_ref()))
    }

    /// Adds `image` after the newest user message when `pictured`, the coordinator's answer that
    /// this request's route may carry it now; otherwise it refuses, so no adapter ever has an
    /// image to drop.
    fn insert_image(&mut self, image: Jpeg, pictured: bool) -> Result<(), ImageRefused> {
        if !pictured || self.choice.is_none() {
            return Err(ImageRefused::Route);
        }
        let at = self.turn()?;
        self.input.insert(at + 1, InputItem::Image(image));
        Ok(())
    }

    /// Adds a hold's screen after the newest user message: the image and the line saying what it
    /// is, then for a screen hold's path the line saying where it was painted, or the line saying
    /// why it isn't there. Each flow has its own lines. An image goes only when `pictured`
    /// ([`Self::insert_image`]).
    pub(crate) fn add_screen(
        &mut self,
        screen: &Screen,
        pictured: bool,
    ) -> Result<(), ImageRefused> {
        let id = match screen {
            Screen::Image(_) => "screen-snapshot",
            Screen::Picture(..) => "screen-picture",
            Screen::PictureNotIncluded(ScreenClass::TooLarge) => "screen-picture-too-large",
            Screen::PictureNotIncluded(ScreenClass::Changed) => "screen-display-changed",
            // A screen hold never closes with the last three, which describe one window.
            Screen::NotIncluded(ScreenClass::NotReady)
            | Screen::PictureNotIncluded(
                ScreenClass::NotReady
                | ScreenClass::NothingThere
                | ScreenClass::CantShare
                | ScreenClass::Unreadable,
            ) => "screen-not-ready",
            Screen::NotIncluded(ScreenClass::NothingThere) => "screen-nothing-there",
            Screen::NotIncluded(ScreenClass::CantShare) => "screen-cant-share",
            Screen::NotIncluded(ScreenClass::Unreadable) => "screen-unreadable",
            Screen::NotIncluded(ScreenClass::TooLarge) => "screen-too-large",
            Screen::NotIncluded(ScreenClass::Changed) => "screen-changed",
            Screen::NotIncluded(ScreenClass::ModelCantSee)
            | Screen::PictureNotIncluded(ScreenClass::ModelCantSee) => "screen-model-cant-see",
            Screen::Withdrawn => return Err(ImageRefused::Withdrawn),
        };
        let size = match screen {
            Screen::Picture(_, _, geometry) => {
                let mut args = fluent_bundle::FluentArgs::new();
                args.set("width", geometry.picture[0]);
                args.set("height", geometry.picture[1]);
                Some(args)
            }
            _ => None,
        };
        let mut lines = vec![screen_line(id, size.as_ref()).ok_or(ImageRefused::Unbundled)?];
        if let Screen::Picture(_, Some(ink), _) = screen {
            let mut args = fluent_bundle::FluentArgs::new();
            for (name, value) in [
                ("left", ink.left),
                ("top", ink.top),
                ("right", ink.right),
                ("bottom", ink.bottom),
            ] {
                args.set(name, value);
            }
            lines.push(screen_line("screen-pointed", Some(&args)).ok_or(ImageRefused::Unbundled)?);
        }
        let after = match screen {
            Screen::Image(image) | Screen::Picture(image, ..) => {
                self.insert_image(image.clone(), pictured)?;
                2
            }
            _ => 1,
        };
        let at = self.turn()? + after;
        for (offset, text) in lines.into_iter().enumerate() {
            self.input.insert(
                at + offset,
                InputItem::Message {
                    role: Role::Developer,
                    text,
                },
            );
        }
        Ok(())
    }

    /// The newest user message's position.
    fn turn(&self) -> Result<usize, ImageRefused> {
        self.input
            .iter()
            .rposition(|item| {
                matches!(
                    item,
                    InputItem::Message {
                        role: Role::User,
                        ..
                    }
                )
            })
            .ok_or(ImageRefused::NoTurn)
    }
}

/// Why a hold's screen part closed without an image: the shell's class, so the line the model
/// gets and the one the user sees can't drift.
pub(crate) use kumochi_protocol::message::ScreenClass;

/// Whether a request on `choice` lists the drawings on screen, and a live check's request takes
/// an image: only the Responses route's.
// shortcut: a Claude run that carries a picture still gets the drawings line without screen
// scenes; carry the run's picture answer into the build before a Claude route is switched on.
fn takes_images(choice: Option<&ModelChoice>) -> bool {
    choice.is_some_and(|choice| choice.connection == ConnectionKind::ChatGpt)
}

/// Where a screen hold's picture was taken: the display's place and size on the desktop in
/// physical pixels, its DPI, and the picture's size. Numbers about the monitor and nothing on
/// it; its `Debug` shows none of them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub(crate) display: kumochi_scene::record::ScreenDisplay,
    pub(crate) picture: [u32; 2],
}

#[cfg(test)]
impl Geometry {
    /// A 1280 by 720 picture of a 2560 by 1440 display at the desktop's origin, at 100%.
    pub(crate) const FIXTURE: Self = Self {
        display: kumochi_scene::record::ScreenDisplay {
            origin: [0, 0],
            size: [2560, 1440],
            dpi: 96,
        },
        picture: [1280, 720],
    };
}

impl fmt::Debug for Geometry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Geometry(..)")
    }
}

/// Why a run's `draw` can't mark the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoCanvas {
    /// No picture of the screen went with the run: another connection, a typed turn, a window
    /// hold, or a screen hold whose picture wasn't included.
    NoPicture,
    /// The picture shows a display that doesn't start at the desktop's origin, which the
    /// overlay doesn't cover.
    OtherDisplay,
    /// The user turned off the setting that allowed the picture.
    Off,
    /// The user's screen may have changed since the picture, so the shell skipped a screen scene
    /// drawn on it.
    Changed,
}

/// What a hold's screen part hands its run, which every request of the run carries.
#[derive(Clone)]
pub(crate) enum Screen {
    Image(Jpeg),
    /// A screen hold's picture, where its pointer's path was painted on it, and where it was
    /// taken.
    Picture(Jpeg, Option<crate::screen::InkBounds>, Geometry),
    NotIncluded(ScreenClass),
    /// Why a screen hold's picture isn't there.
    PictureNotIncluded(ScreenClass),
    /// The image the user withdrew by turning screen holds off; no request may carry the run on.
    Withdrawn,
}

impl Screen {
    /// What the run's turn carries, for its stored row and the shell.
    pub(crate) fn held(&self) -> kumochi_protocol::message::HeldScreen {
        match self {
            Self::Image(_) => kumochi_protocol::message::HeldScreen::Attached,
            Self::Picture(..) => kumochi_protocol::message::HeldScreen::Screen,
            Self::NotIncluded(class) | Self::PictureNotIncluded(class) => {
                kumochi_protocol::message::HeldScreen::NotIncluded(*class)
            }
            Self::Withdrawn => kumochi_protocol::message::HeldScreen::Withdrawn,
        }
    }

    /// The picture a screen scene of the run is drawn on: a screen hold's, of the display at the
    /// desktop's origin, on a route that may carry it. `pictured` is the same answer
    /// [`ModelRequest::add_screen`] took, so a scene is never drawn on a picture that didn't go.
    /// # Errors
    /// [`NoCanvas`] says why not.
    pub(crate) fn canvas(screen: Option<&Self>, pictured: bool) -> Result<Geometry, NoCanvas> {
        let geometry = match screen {
            Some(Self::Picture(_, _, geometry)) => *geometry,
            Some(Self::Withdrawn) => return Err(NoCanvas::Off),
            _ => return Err(NoCanvas::NoPicture),
        };
        if !pictured {
            return Err(NoCanvas::NoPicture);
        }
        if geometry.display.origin != [0, 0] {
            return Err(NoCanvas::OtherDisplay);
        }
        Ok(geometry)
    }
}

impl fmt::Debug for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Image(image) => f.debug_tuple("Image").field(image).finish(),
            Self::Picture(image, ink, geometry) => f
                .debug_tuple("Picture")
                .field(image)
                .field(ink)
                .field(geometry)
                .finish(),
            Self::NotIncluded(class) => f.debug_tuple("NotIncluded").field(class).finish(),
            Self::PictureNotIncluded(class) => {
                f.debug_tuple("PictureNotIncluded").field(class).finish()
            }
            Self::Withdrawn => f.write_str("Withdrawn"),
        }
    }
}

/// A fixed developer line from `locales/en/brain.ftl`; `None` only if the bundled messages are
/// broken.
fn screen_line(id: &str, args: Option<&fluent_bundle::FluentArgs<'_>>) -> Option<String> {
    let resource = fluent_bundle::FluentResource::try_new(
        include_str!("../../../../locales/en/brain.ftl").to_owned(),
    )
    .ok()?;
    let mut bundle =
        fluent_bundle::FluentBundle::<fluent_bundle::FluentResource>::new(vec!["en".parse().ok()?]);
    bundle.set_use_isolating(false);
    bundle.add_resource(resource).ok()?;
    let pattern = bundle.get_message(id)?.value()?;
    let mut errors = Vec::new();
    let line = bundle
        .format_pattern(pattern, args, &mut errors)
        .into_owned();
    errors.is_empty().then_some(line)
}

/// A request and every thread whose raw text it carries, for the coordinator's delete check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Built {
    pub(crate) request: ModelRequest,
    /// Sorted, each once.
    pub(crate) sources: Vec<ThreadId>,
}

/// Why no request was built.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BuildError {
    /// The thread is being deleted or is gone, so it sends nothing.
    #[error("the thread is being deleted or is gone")]
    Deleting,
    /// The run's origin doesn't match the thread it runs in.
    #[error("the run's origin doesn't match its thread")]
    Origin,
    /// The run's turn isn't in the store.
    #[error("the run's turn isn't stored")]
    NoTurn,
    /// A tool call and its result are apart, or an item is repeated.
    #[error("a tool call is split from its result, or an item repeats")]
    InvalidCut,
    /// Even the current run and the summary are over the bound. `room` is what the bound leaves
    /// for a summary after the current run, when the current run fits without one.
    #[error("the request is over its size bound")]
    TooLarge {
        /// Bytes left for a summary; `None` when the current run alone doesn't fit.
        room: Option<usize>,
    },
    /// The read failed.
    #[error("can't read the context")]
    Store(#[from] StoreError),
}
