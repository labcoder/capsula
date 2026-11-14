//! A fake desktop for the dock line's tests, the machine's and the view's.

use super::{Attempt, Line, LineFrame, Placement, Surfaces};
use crate::presence::geometry::Layout;

/// The dock at x, with its overlay over its slot.
pub(crate) fn placement(x: i32, index: usize) -> Placement {
    let slot = [x, 0, 58, 21];
    Placement {
        layout: Layout {
            dock: slot,
            panel: [x, 30, 300, 140],
            overlay: slot,
        },
        frame: LineFrame::in_slot(slot, 1.0),
        index,
    }
}

/// Windows as the surfaces see it: each call refuses when its script says so, and changes
/// nothing then.
#[derive(Default)]
pub(crate) struct Fake {
    pub(crate) mask: Option<LineFrame>,
    /// A pull's mask over this area, which the line's surfaces didn't give.
    pub(crate) pulled: Option<[i32; 4]>,
    pub(crate) overlay: Option<[i32; 4]>,
    pub(crate) shown: Option<usize>,
    pub(crate) refuse_uploads: Vec<bool>,
    pub(crate) refuse_moves: Vec<bool>,
    pub(crate) refuse_shows: Vec<bool>,
    pub(crate) refuse_overlay_hides: bool,
    pub(crate) refuse_proxy_hides: bool,
    pub(crate) refuse_clears: bool,
}

fn refused(script: &mut Vec<bool>) -> bool {
    !script.is_empty() && script.remove(0)
}

impl Surfaces for Fake {
    type Error = &'static str;

    fn upload(&mut self, frame: &LineFrame) -> Result<(), &'static str> {
        if refused(&mut self.refuse_uploads) {
            return Err("upload refused");
        }
        self.mask = Some(*frame);
        self.pulled = None;
        Ok(())
    }

    fn move_overlay(&mut self, overlay: [i32; 4]) -> Result<(), &'static str> {
        if refused(&mut self.refuse_moves) {
            return Err("move refused");
        }
        self.overlay = Some(overlay);
        Ok(())
    }

    fn show(&mut self, index: usize) -> Result<(), &'static str> {
        if refused(&mut self.refuse_shows) {
            return Err("show refused");
        }
        self.shown = Some(index);
        Ok(())
    }

    fn hide_overlay(&mut self) -> Result<(), &'static str> {
        if self.refuse_overlay_hides {
            return Err("overlay hide refused");
        }
        self.overlay = None;
        Ok(())
    }

    fn hide_proxy(&mut self) -> Result<(), &'static str> {
        if self.refuse_proxy_hides {
            return Err("proxy hide refused");
        }
        self.shown = None;
        Ok(())
    }

    fn clear_mask(&mut self, hit: [i32; 4]) -> Result<(), &'static str> {
        if self.refuse_clears {
            return Err("clear refused");
        }
        if self.pulled == Some(hit) {
            self.pulled = None;
            return Ok(());
        }
        if self.mask.map(|mask| mask.hit) != Some(hit) {
            return Err("cleared a band it doesn't hold");
        }
        self.mask = None;
        Ok(())
    }
}

impl Fake {
    /// The line on screen at `placement`, with the band under it and the proxy showing it.
    pub(crate) fn holds(&self, placement: Placement) -> bool {
        self.mask == Some(placement.frame)
            && self.overlay == Some(placement.layout.overlay)
            && self.shown == Some(placement.index)
    }

    /// Nothing on screen and nothing taking clicks.
    pub(crate) fn hidden(&self) -> bool {
        self.overlay.is_none() && self.shown.is_none()
    }

    /// A shown proxy with a band: it takes clicks.
    pub(crate) fn takes_clicks(&self) -> bool {
        self.shown.is_some() && (self.mask.is_some() || self.pulled.is_some())
    }
}

/// A line at A on a fake desktop.
pub(crate) fn at_a() -> (Line, Fake) {
    let (mut line, mut fake) = (Line::default(), Fake::default());
    line.request(placement(0, 0));
    assert_eq!(line.attempt(&mut fake), Attempt::Placed(placement(0, 0)));
    (line, fake)
}

pub(crate) fn failed([retry, closed]: [bool; 2]) -> impl Fn(&Attempt<&'static str>) -> bool {
    move |attempt| matches!(attempt, Attempt::Failed { retry: r, closed: c, .. } if *r == retry && *c == closed)
}
