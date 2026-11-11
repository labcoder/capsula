//! Optional local voice: models installed only on an explicit request, speech input and spoken
//! replies.

pub(crate) mod download;
pub(crate) mod installer;
pub(crate) mod pump;
pub(crate) mod reply;
pub(crate) mod resample;
pub(crate) mod segment;
pub(crate) mod stt;
pub(crate) mod tts;

/// What became of one load ask. An ask is the installer's token, so a report for an older ask
/// can never be mistaken for the current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoadEvent {
    /// Loaded and warmed up, and still the newest ask when it finished.
    Loaded,
    /// The load failed.
    Failed,
    /// It finished after a newer ask or its unload, so it was freed unused.
    Discarded,
    /// Its model is freed, or the owner never had it.
    Unloaded,
    /// The worker was lost while the ask was loading or loaded.
    Lost,
    /// The owner stopped without proof that its model was released.
    Unavailable,
    /// The model path was refused before construction; no model remains held.
    ModelPath,
}

/// One ask's outcome, as an owner thread reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoadReport {
    pub ask: u64,
    pub event: LoadEvent,
}

/// Where an owner thread sends its reports; a closed receiver drops them.
#[derive(Clone, Debug)]
pub(crate) struct Reports(tokio::sync::mpsc::UnboundedSender<LoadReport>);

impl Reports {
    pub(crate) fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<LoadReport>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (Self(sender), receiver)
    }

    fn send(&self, ask: u64, event: LoadEvent) {
        self.0.send(LoadReport { ask, event }).ok();
    }
}
