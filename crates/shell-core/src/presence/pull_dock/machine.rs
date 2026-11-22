use crate::presence::frame_rate::FrameLoop;
use crate::presence::line::machine::LineFrame;
use crate::presence::pull_picture::machine::{RowFull, Rows};

/// Whether the dock has rendered the picture it accepted last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drawing {
    Drawn,
    /// It may not be on screen yet.
    Pending,
}

/// What one dock frame did with the next picture.
#[derive(Debug, PartialEq, Eq)]
pub enum Commit<E> {
    /// The dock hasn't drawn the picture it accepted last, so nothing new is uploaded.
    Waiting,
    /// Windows holds the mask, so the dock draws the picture now.
    Accepted,
    /// More spans than a mask row holds; the dock keeps its picture.
    Full(RowFull),
    /// Windows refused the mask; the dock keeps its picture.
    Failed(E),
}

/// What the dock holds of a pull, as a resting pull gives the line the dock back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holding {
    /// A picture, drawn or not.
    Picture,
    /// No picture, but the frame without it isn't drawn yet.
    Clearing,
    Nothing,
}

/// What a resting pull does at a dock frame to give the line the dock back.
#[derive(Debug, PartialEq, Eq)]
pub enum Rest {
    /// The dock still draws the body: it stops, and the peek shows it.
    Release,
    /// That frame isn't drawn yet, or the line has been asked already.
    Wait,
    /// The dock draws nothing of the pull: the line may take its mask and overlay back.
    Reclaim,
}

/// The dock's state during a pull. Its rows are sized once, so a frame grows none.
#[derive(Debug, Default)]
pub struct PullDock {
    pub frames: FrameLoop,
    /// Where the overlay is, while a pull holds it.
    pub area: Option<[i32; 4]>,
    /// The pixels of the picture the dock accepted last, which it draws or has drawn.
    pub shown: Rows,
    /// The next upload's rows.
    mask: Rows,
    /// The line has been asked to take the dock back.
    pub reclaiming: bool,
}

impl PullDock {
    /// Rows for a pull area up to `rows` tall.
    pub fn new(rows: usize) -> Self {
        Self {
            shown: Rows::with_capacity(rows),
            mask: Rows::with_capacity(rows),
            ..Self::default()
        }
    }

    /// A resting pull's next step. The line's mask is its band alone, so it may take over only
    /// once the dock has drawn a frame with no body in it.
    pub fn rest(&mut self, holding: Holding) -> Rest {
        match holding {
            Holding::Picture => Rest::Release,
            Holding::Clearing => Rest::Wait,
            Holding::Nothing if self.reclaiming => Rest::Wait,
            Holding::Nothing => {
                self.reclaiming = true;
                Rest::Reclaim
            }
        }
    }

    /// Uploads the mask for the next picture, `area` with pixels `next`, over `line`:
    /// the line's band, `next` and the picture shown now. Once accepted, `next` is shown.
    /// While the last accepted picture's drawing is pending, nothing is uploaded: the mask
    /// must keep owning it.
    pub fn commit<E>(
        &mut self,
        drawing: Drawing,
        line: &LineFrame,
        area: [i32; 4],
        next: &mut Rows,
        upload: impl FnOnce([i32; 4], &Rows) -> Result<(), E>,
    ) -> Commit<E> {
        if drawing == Drawing::Pending {
            return Commit::Waiting;
        }
        let mask = &mut self.mask;
        mask.clear(area[1], usize::try_from(area[3]).unwrap_or(0));
        let [_, top, _, height] = line.hit;
        let owned = (top..top + height)
            .try_for_each(|y| {
                line.row_spans(y)
                    .into_iter()
                    .flatten()
                    .try_for_each(|span| mask.push(y, span))
            })
            .and_then(|()| mask.extend(next))
            .and_then(|()| mask.extend(&self.shown));
        if let Err(full) = owned {
            return Commit::Full(full);
        }
        if let Err(error) = upload(area, &self.mask) {
            return Commit::Failed(error);
        }
        std::mem::swap(next, &mut self.shown);
        Commit::Accepted
    }
}

#[cfg(test)]
mod tests {
    use super::{Commit, Drawing, Holding, PullDock, Rest};
    use crate::presence::line::machine::LineFrame;
    use crate::presence::pull_picture::machine::Rows;

    const AREA: [i32; 4] = [0, 0, 200, 40];

    /// A picture owning `[left, right)` on rows 30 and 31, below the line's band.
    fn picture(left: i32, right: i32) -> Rows {
        let mut rows = Rows::with_capacity(40);
        rows.clear(0, 40);
        for y in [30, 31] {
            rows.push(y, [left, right]).expect("room");
        }
        rows
    }

    /// Commits `next`, returning what happened and the mask Windows was given, if any.
    fn commit(
        dock: &mut PullDock,
        drawing: Drawing,
        next: &mut Rows,
        upload: Result<(), &'static str>,
    ) -> (Commit<&'static str>, Option<Rows>) {
        let line = LineFrame::in_slot([0, 0, 58, 21], 1.0);
        let mut given = None;
        let commit = dock.commit(drawing, &line, AREA, next, |_, mask| {
            given = Some(mask.clone());
            upload
        });
        (commit, given)
    }

    #[test]
    fn a_picture_accepted_but_not_drawn_holds_back_the_next_until_it_is() {
        let mut dock = PullDock::new(40);
        let (mut p1, mut p2, mut p3) = (picture(100, 110), picture(140, 150), picture(170, 180));
        assert_eq!(
            commit(&mut dock, Drawing::Drawn, &mut p1, Ok(())).0,
            Commit::Accepted
        );
        assert_eq!(
            commit(&mut dock, Drawing::Pending, &mut p2, Ok(())),
            (Commit::Waiting, None),
            "P1 may not be on screen yet: no mask leaves it unowned"
        );
        let (refused, given) = commit(&mut dock, Drawing::Drawn, &mut p2, Err("refused"));
        assert_eq!(refused, Commit::Failed("refused"));
        let given = given.expect("tried");
        assert_eq!(
            given.row(30),
            [Some([100, 110]), Some([140, 150]), None, None],
            "P2 was offered beside the shown P1"
        );
        let (accepted, given) = commit(&mut dock, Drawing::Drawn, &mut p3, Ok(()));
        assert_eq!(accepted, Commit::Accepted);
        let given = given.expect("uploaded");
        assert_eq!(
            given.row(30),
            [Some([100, 110]), Some([170, 180]), None, None],
            "P1, still drawn after P2 was refused, stays owned beside P3"
        );
        assert_eq!(
            given.row(5),
            [Some([1, 57]), None, None, None],
            "and the line's band"
        );
    }

    #[test]
    fn a_mask_with_a_fifth_span_in_a_row_is_never_uploaded() {
        let mut dock = PullDock::new(40);
        let mut wide = Rows::with_capacity(40);
        wide.clear(0, 40);
        for left in [100, 120, 140, 160] {
            wide.push(30, [left, left + 5]).expect("room for four");
        }
        assert_eq!(
            commit(&mut dock, Drawing::Drawn, &mut wide, Ok(())).0,
            Commit::Accepted
        );
        let mut next = picture(180, 190);
        let (full, given) = commit(&mut dock, Drawing::Drawn, &mut next, Ok(()));
        assert!(matches!(full, Commit::Full(_)));
        assert_eq!(given, None);
    }

    #[test]
    fn a_resting_pull_gives_the_line_the_dock_only_after_a_frame_without_the_body() {
        let mut dock = PullDock::new(40);
        assert_eq!(
            dock.rest(Holding::Picture),
            Rest::Release,
            "the dock still draws the body"
        );
        assert_eq!(
            dock.rest(Holding::Clearing),
            Rest::Wait,
            "the frame without it isn't drawn yet"
        );
        assert_eq!(dock.rest(Holding::Nothing), Rest::Reclaim);
        assert_eq!(dock.rest(Holding::Nothing), Rest::Wait, "asked once");
    }
}
