//! A build without the `gate-recorder` feature has no gate observer. Launch refuses
//! `--gate-observer`, so none is ever made, and every hook here does nothing.

use std::io;

use gpui_kit::{App, Global};
use kumochi_protocol::constants::LIVE_CHATGPT_TEXT_ENV;
use kumochi_protocol::message::{MAX_VISIBLE_SCENES, SceneId};

use super::Launch;
use super::geometry::PanelVisibility;
use super::line::LineFrame;
use crate::platform::{MaskUpdate, PresenceError};

type Observations = [Option<(SceneId, u64, u32)>; MAX_VISIBLE_SCENES];

/// Never made in this build.
#[derive(Debug)]
pub(crate) enum Observer {}

impl Global for Observer {}

impl Observer {
    #[cfg(debug_assertions)]
    pub(super) fn take_draw_fixture(_: &mut App) -> Option<&'static str> {
        None
    }

    /// No observer, and no drawing replay, which needs one.
    ///
    /// # Errors
    /// Fails when the drawing replay is asked for.
    pub(crate) fn prepare(_: &Launch) -> io::Result<Option<Self>> {
        if std::env::var(LIVE_CHATGPT_TEXT_ENV).ok().as_deref() == Some("draw") {
            return Err(io::Error::other("drawing replay is not compiled in"));
        }
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Drawn(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Surface {
    Dock,
    Panel,
}

pub(super) fn active(_: &App) -> bool {
    false
}

pub(super) fn begin(_: &App) -> i64 {
    0
}

pub(super) fn commit(
    _: &mut App,
    _: i64,
    _: Option<&LineFrame>,
    _: [i32; 4],
    _: &Result<MaskUpdate, PresenceError>,
) -> Drawn {
    Drawn(())
}

pub(super) fn hover(_: &mut App, _: PanelVisibility, _: Option<i64>) {}

pub(in crate::presence) fn overlay_painted(_: &App, _: &Observations, _: &Observations) {}

/// Never made in this build.
#[derive(Debug)]
pub(crate) enum Closing {}

impl Closing {
    pub(crate) fn finish(self) {
        match self {}
    }
}

pub(crate) fn release(_: &mut App) -> Option<Closing> {
    None
}
