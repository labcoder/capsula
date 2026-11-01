//! Incremental typed observer records and bounded submission-to-display gate summaries.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context as _, Result, bail, ensure};
use capsula_protocol::constants::{INPUT_SESSION_MARKER, OBSERVER_REPAINT_MESSAGE};

mod summary;
pub(super) use summary::{summarize_click, summarize_keyboard};

#[derive(Debug, Clone, Copy)]
pub(super) struct Startup {
    pub(super) panel_hwnd: usize,
    pub(super) frequency: i64,
    pub(super) work_area: [i32; 4],
    pub(super) dock: [i32; 4],
    pub(super) overlay: [i32; 4],
    pub(super) panel: [i32; 4],
}

#[derive(Debug)]
pub(super) struct Geometry {
    pub(super) origin_y: i32,
    pub(super) rows: Vec<Row>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) drawn: Option<[i32; 2]>,
    pub(super) owned: [Option<[i32; 2]>; 2],
}

impl Geometry {
    fn row(&self, y: i32) -> Option<&Row> {
        self.rows
            .get(usize::try_from(i64::from(y) - i64::from(self.origin_y)).ok()?)
    }

    pub(super) fn contains_drawn(&self, [x, y]: [i32; 2]) -> bool {
        self.row(y)
            .and_then(|row| row.drawn)
            .is_some_and(|[left, right]| (left..right).contains(&x))
    }

    pub(super) fn contains_owned(&self, [x, y]: [i32; 2]) -> bool {
        self.row(y).is_some_and(|row| {
            row.owned
                .iter()
                .flatten()
                .any(|[left, right]| (*left..*right).contains(&x))
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Commit {
    pub(super) begin_qpc: i64,
    pub(super) qpc: i64,
    pub(super) epoch: u64,
    pub(super) geometry: u64,
    pub(super) proxy: [i32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Dock,
    Panel,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Frame {
    pub(super) id: u64,
    pub(super) paint_qpc: i64,
    pub(super) completion_qpc: Option<i64>,
    pub(super) kind: Kind,
    pub(super) hover: u64,
    pub(super) geometry: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Click {
    pub(super) qpc: i64,
    pub(super) point: [i32; 2],
    pub(super) epoch: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Hover {
    pub(super) id: u64,
    pub(super) qpc: i64,
    pub(super) visible: bool,
    pub(super) click_qpc: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StopTarget {
    pub(super) frame: u64,
    pub(super) qpc: i64,
    pub(super) hover: u64,
    pub(super) rect: [i32; 4],
}

#[derive(Debug, Default)]
pub(super) struct Observation {
    pub(super) startup: Option<Startup>,
    pub(super) geometries: BTreeMap<u64, Geometry>,
    pub(super) commits: Vec<Commit>,
    pub(super) frames: Vec<Frame>,
    pub(super) clicks: Vec<Click>,
    pub(super) hovers: Vec<Hover>,
    pub(super) stop_target: Option<StopTarget>,
    pending_targets: Vec<StopTarget>,
    pid: u32,
    lines: u64,
    closed: bool,
    ended: bool,
    last_repaint: i64,
    last_completion: i64,
}

#[derive(Debug)]
pub(super) struct Reader {
    file: File,
    observation: Observation,
    pending: Vec<u8>,
    offset: u64,
    failed: bool,
}

impl Reader {
    pub(super) fn open(path: &Path, pid: u32) -> Result<Self> {
        ensure!(pid > 0, "invalid observer PID");
        Ok(Self {
            file: File::open(path).context("opening observer file")?,
            observation: Observation {
                pid,
                ..Observation::default()
            },
            pending: Vec::new(),
            offset: 0,
            failed: false,
        })
    }

    pub(super) fn update(&mut self) -> Result<&Observation> {
        ensure!(!self.failed, "observer reader previously failed");
        if let Err(error) = self.advance() {
            self.failed = true;
            return Err(error);
        }
        Ok(&self.observation)
    }

    fn advance(&mut self) -> Result<()> {
        ensure!(
            self.file.metadata()?.len() >= self.offset,
            "observer file was truncated"
        );
        let mut bytes = [0_u8; 8192];
        loop {
            let count = self.file.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            self.offset += u64::try_from(count)?;
            self.pending.extend_from_slice(&bytes[..count]);
            let mut consumed = 0;
            while let Some(end) = self.pending[consumed..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                ensure!(end <= 65536, "observer line exceeds bounded row format");
                let end = consumed + end;
                let line = std::str::from_utf8(&self.pending[consumed..end])?;
                self.observation
                    .line(line.strip_suffix('\r').unwrap_or(line))
                    .with_context(|| format!("observer line {}", self.observation.lines + 1))?;
                consumed = end + 1;
            }
            self.pending.drain(..consumed);
            ensure!(
                self.pending.len() <= 65536,
                "observer partial line exceeds bound"
            );
        }
        Ok(())
    }

    fn complete(&self) -> Result<()> {
        ensure!(
            !self.failed && self.pending.is_empty(),
            "invalid or truncated observer file"
        );
        ensure!(
            self.observation.startup.is_some() && self.observation.closed && self.observation.ended,
            "observer startup/close/end missing"
        );
        ensure!(
            self.observation.pending_targets.is_empty(),
            "Stop target lacks a completed Panel frame"
        );
        Ok(())
    }
}

pub(super) fn read(path: &Path, pid: u32) -> Result<Observation> {
    let (observation, validation) = load(path, pid);
    validation?;
    Ok(observation)
}

fn load(path: &Path, pid: u32) -> (Observation, Result<()>) {
    let mut reader = match Reader::open(path, pid) {
        Ok(reader) => reader,
        Err(error) => return (Observation::default(), Err(error)),
    };
    let validation = reader.update().map(|_| ()).and_then(|()| reader.complete());
    (reader.observation, validation)
}

impl Observation {
    fn line(&mut self, line: &str) -> Result<()> {
        ensure!(!self.ended, "observer record after end");
        let (kind, text) = line.split_once(' ').context("invalid observer envelope")?;
        ensure!(!self.closed || kind == "end", "observer record after close");
        match kind {
            "startup" => self.accept_startup(text)?,
            "geometry" => self.accept_geometry(text)?,
            "commit" => self.accept_commit(text)?,
            "paint" => self.accept_paint(text)?,
            "drawn" => self.accept_drawn(text)?,
            "target" => self.accept_target(text)?,
            "hover" => {
                let (id, counter, visible, click) =
                    if text.split(' ').any(|field| field.starts_with("click_qpc=")) {
                        let [id, counter, visible, click] =
                            fields(text, ["id", "qpc", "visible", "click_qpc"])?;
                        (id, counter, visible, number::<i64>(click)?)
                    } else {
                        let [id, counter, visible] = fields(text, ["id", "qpc", "visible"])?;
                        (id, counter, visible, 0)
                    };
                let id = number(id)?;
                next_id(id, self.hovers.len())?;
                let qpc = ordered(counter, self.hovers.last().map_or(0, |value| value.qpc))?;
                ensure!((0..=qpc).contains(&click), "invalid panel click origin");
                self.hovers.push(Hover {
                    id,
                    qpc,
                    visible: boolean(visible)?,
                    click_qpc: (click != 0).then_some(click),
                });
            }
            "click" => self.accept_click(text)?,
            "repaint" => {
                let [counter] = fields(text, ["qpc"])?;
                self.last_repaint = ordered(counter, self.last_repaint)?;
            }
            "close" => self.accept_close(text)?,
            "end" => {
                let [lines, errors] = fields(text, ["lines", "write_errors"])?;
                ensure!(
                    self.closed
                        && number::<u64>(lines)? == self.lines
                        && number::<u64>(errors)? == 0,
                    "invalid observer end counters"
                );
                self.ended = true;
                return Ok(());
            }
            _ => bail!("unknown observer record"),
        }
        self.lines += 1;
        Ok(())
    }

    fn accept_startup(&mut self, text: &str) -> Result<()> {
        let [
            version,
            pid,
            dock_hwnd,
            panel_hwnd,
            proxy_hwnd,
            scale,
            position,
            work,
            dock,
            overlay,
            panel,
            frequency,
            cap,
            queue,
            repaint,
            marker,
        ] = fields(
            text,
            [
                "version",
                "pid",
                "dock_hwnd",
                "panel_hwnd",
                "proxy_hwnd",
                "scale",
                "position",
                "work_area",
                "dock",
                "overlay",
                "panel",
                "qpc_frequency",
                "row_cap",
                "queue",
                "repaint_message",
                "marker",
            ],
        )?;
        ensure!(
            self.startup.is_none() && version == "1" && number::<u32>(pid)? == self.pid,
            "duplicate startup or wrong version/PID"
        );
        let handles = [hex(dock_hwnd)?, hex(panel_hwnd)?, hex(proxy_hwnd)?];
        ensure!(
            handles.iter().all(|handle| *handle > 0)
                && handles[0] != handles[1]
                && handles[0] != handles[2]
                && handles[1] != handles[2],
            "invalid observer HWNDs"
        );
        let scale: f64 = scale.parse()?;
        ensure!(scale.is_finite() && scale > 0.0, "invalid observer scale");
        ensure!(
            matches!(
                position,
                "TopLeft"
                    | "TopCenter"
                    | "TopRight"
                    | "MiddleLeft"
                    | "Center"
                    | "MiddleRight"
                    | "BottomLeft"
                    | "BottomCenter"
                    | "BottomRight"
            ),
            "invalid observer position"
        );
        ensure!(
            cap == "512"
                && queue == "256"
                && hex(repaint)? == u64::from(OBSERVER_REPAINT_MESSAGE)
                && hex(marker)? == u64::try_from(INPUT_SESSION_MARKER)?,
            "observer constants mismatch"
        );
        let frequency = positive(frequency)?;
        let startup = Startup {
            panel_hwnd: usize::try_from(handles[1])?,
            frequency,
            work_area: rect(work)?,
            dock: rect(dock)?,
            overlay: rect(overlay)?,
            panel: rect(panel)?,
        };
        for target in &self.pending_targets {
            ensure!(
                inside_panel(startup.panel, target.rect),
                "Stop target is outside startup panel"
            );
        }
        self.pending_targets.retain(|target| {
            self.frames[usize::try_from(target.frame - 1).expect("validated frame index")]
                .completion_qpc
                .is_none()
        });
        self.startup = Some(startup);
        Ok(())
    }

    fn accept_geometry(&mut self, text: &str) -> Result<()> {
        let [id, origin, count, trimmed, spans] =
            fields(text, ["id", "origin_y", "rows", "trimmed", "spans"])?;
        let id = number(id)?;
        next_id(id, self.geometries.len())?;
        let count: usize = number(count)?;
        ensure!(count <= 512, "observer geometry overflow");
        let origin_y: i32 = number(origin)?;
        origin_y
            .checked_add(i32::try_from(count)?)
            .context("geometry Y overflow")?;
        let rows = if count == 0 && spans.is_empty() {
            Vec::new()
        } else {
            spans
                .split(';')
                .map(|text| {
                    let parts: Vec<_> = text.split(',').collect();
                    ensure!(parts.len() == 3, "invalid geometry row");
                    Ok(Row {
                        drawn: span(parts[0])?,
                        owned: [span(parts[1])?, span(parts[2])?],
                    })
                })
                .collect::<Result<Vec<_>>>()?
        };
        ensure!(rows.len() == count, "geometry row count mismatch");
        let geometry = Geometry { origin_y, rows };
        let previous = if boolean(trimmed)? {
            None
        } else {
            Some(
                self.geometries
                    .last_key_value()
                    .context("untrimmed geometry has no predecessor")?
                    .1,
            )
        };
        validate_union(&geometry, previous)?;
        self.geometries.insert(id, geometry);
        Ok(())
    }

    fn accept_commit(&mut self, text: &str) -> Result<()> {
        let [
            seq,
            begin,
            counter,
            result,
            proposal,
            epoch,
            geometry,
            proxy,
        ] = fields(
            text,
            [
                "seq",
                "begin_qpc",
                "qpc",
                "result",
                "proposal",
                "epoch",
                "geometry",
                "proxy",
            ],
        )?;
        next_id(number(seq)?, self.commits.len())?;
        ensure!(
            matches!(result, "shape" | "moved" | "unchanged"),
            "failed or unknown mask commit"
        );
        let (proposal, epoch, geometry) = (
            number::<u64>(proposal)?,
            number::<u64>(epoch)?,
            number::<u64>(geometry)?,
        );
        ensure!(
            proposal == geometry
                && self.geometries.contains_key(&geometry)
                && self
                    .geometries
                    .last_key_value()
                    .is_some_and(|(id, _)| *id == proposal),
            "commit geometry reference mismatch"
        );
        let previous = self.commits.last();
        let expected_epoch = if previous.is_some_and(|value| value.geometry == geometry) {
            previous.map_or(0, |value| value.epoch)
        } else {
            previous
                .map_or(0, |value| value.epoch)
                .checked_add(1)
                .context("mask epoch overflow")?
        };
        ensure!(epoch == expected_epoch, "invalid accepted mask epoch");
        let qpc = ordered(counter, previous.map_or(0, |value| value.qpc))?;
        let begin_qpc = positive(begin)?;
        ensure!(
            begin_qpc <= qpc && previous.is_none_or(|value| begin_qpc >= value.qpc),
            "invalid mask upload interval"
        );
        self.commits.push(Commit {
            begin_qpc,
            qpc,
            epoch,
            geometry,
            proxy: rect(proxy)?,
        });
        Ok(())
    }

    fn accept_paint(&mut self, text: &str) -> Result<()> {
        let [id, counter, kind, hover, epoch, geometry, mask] = fields(
            text,
            [
                "frame",
                "qpc",
                "kind",
                "hover",
                "drawn_epoch",
                "drawn_geometry",
                "mask_epoch",
            ],
        )?;
        let id = number(id)?;
        next_id(id, self.frames.len())?;
        let (hover, epoch, geometry, mask) = (
            number::<u64>(hover)?,
            number::<u64>(epoch)?,
            number::<u64>(geometry)?,
            number::<u64>(mask)?,
        );
        ensure!(
            mask == self.commits.last().map_or(0, |value| value.epoch),
            "paint mask epoch mismatch"
        );
        let kind = match kind {
            "dock" => {
                ensure!(
                    hover == 0
                        && ((epoch == 0 && geometry == 0 && mask == 0)
                            || self
                                .commits
                                .iter()
                                .any(|value| value.epoch == epoch && value.geometry == geometry)),
                    "invalid dock drawn reference"
                );
                Kind::Dock
            }
            "panel" => {
                ensure!(
                    epoch == 0
                        && geometry == 0
                        && hover == self.hovers.last().map_or(0, |value| value.id),
                    "invalid panel hover reference"
                );
                Kind::Panel
            }
            _ => bail!("unknown observer surface"),
        };
        let qpc = ordered(
            counter,
            self.frames.last().map_or(0, |value| value.paint_qpc),
        )?;
        self.frames.push(Frame {
            id,
            paint_qpc: qpc,
            completion_qpc: None,
            kind,
            hover,
            geometry,
        });
        Ok(())
    }

    fn accept_drawn(&mut self, text: &str) -> Result<()> {
        let [id, counter] = fields(text, ["frame", "qpc"])?;
        let id: u64 = number(id)?;
        let index = usize::try_from(id.checked_sub(1).context("zero completed frame")?)?;
        let frame = self
            .frames
            .get_mut(index)
            .context("completion references unknown frame")?;
        let qpc = ordered(counter, self.last_completion)?;
        ensure!(
            frame.completion_qpc.is_none() && qpc > frame.paint_qpc,
            "duplicate or nonpositive completion interval"
        );
        for target in self
            .pending_targets
            .iter()
            .filter(|target| target.frame == id)
        {
            ensure!(
                target.qpc <= qpc,
                "Stop target is outside completed Panel interval"
            );
        }
        frame.completion_qpc = Some(qpc);
        let awaiting_startup = self.startup.is_none();
        self.pending_targets
            .retain(|target| target.frame != id || awaiting_startup);
        self.last_completion = qpc;
        Ok(())
    }

    fn accept_target(&mut self, text: &str) -> Result<()> {
        let [frame, counter, kind, hover, bounds, inside] =
            fields(text, ["frame", "qpc", "kind", "hover", "rect", "inside"])?;
        ensure!(
            kind == "stop" && boolean(inside)?,
            "invalid or out-of-panel observer target"
        );
        let frame: u64 = number(frame)?;
        let index = usize::try_from(frame.checked_sub(1).context("zero target frame")?)?;
        let painted = self
            .frames
            .get(index)
            .context("target references unknown frame")?;
        let hover = number(hover)?;
        let qpc = ordered(counter, self.stop_target.map_or(0, |target| target.qpc))?;
        ensure!(
            painted.kind == Kind::Panel && hover == painted.hover && qpc >= painted.paint_qpc,
            "target Panel/frame/hover reference mismatch"
        );
        ensure!(
            painted.completion_qpc.is_none_or(|end| qpc <= end),
            "Stop target is outside completed Panel interval"
        );
        let rect = rect(bounds)?;
        if let Some(startup) = self.startup {
            ensure!(
                inside_panel(startup.panel, rect),
                "Stop target is outside startup panel"
            );
        }
        let target = StopTarget {
            frame,
            qpc,
            hover,
            rect,
        };
        if self.startup.is_none() || painted.completion_qpc.is_none() {
            self.pending_targets.push(target);
        }
        self.stop_target = Some(target);
        Ok(())
    }

    fn accept_click(&mut self, text: &str) -> Result<()> {
        let [counter, point, epoch, marker] =
            fields(text, ["qpc", "point", "mask_epoch", "marker"])?;
        let epoch = number::<u64>(epoch)?;
        ensure!(
            epoch > 0
                && epoch == self.commits.last().map_or(0, |value| value.epoch)
                && hex(marker)? == u64::try_from(INPUT_SESSION_MARKER)?,
            "invalid tagged click epoch/marker"
        );
        let qpc = ordered(counter, self.clicks.last().map_or(0, |value| value.qpc))?;
        self.clicks.push(Click {
            qpc,
            point: coordinates(point)?,
            epoch,
        });
        Ok(())
    }

    fn accept_close(&mut self, text: &str) -> Result<()> {
        let [commits, frames, dropped, overflow, failed, unposted] = fields(
            text,
            [
                "commits",
                "frames",
                "dropped",
                "overflow",
                "post_failed",
                "unposted",
            ],
        )?;
        ensure!(
            self.startup.is_some()
                && number::<u64>(commits)? == u64::try_from(self.commits.len())?
                && number::<u64>(frames)? == u64::try_from(self.frames.len())?,
            "observer close count mismatch"
        );
        ensure!(
            number::<u64>(dropped)? == 0
                && number::<u64>(overflow)? == 0
                && number::<u64>(failed)? == 0,
            "observer lost or failed records"
        );
        ensure!(
            number::<u64>(unposted)?
                <= u64::try_from(
                    self.frames
                        .iter()
                        .filter(|frame| frame.completion_qpc.is_none())
                        .count()
                )?,
            "invalid unposted frame count"
        );
        self.closed = true;
        Ok(())
    }
}

fn fields<'a, const N: usize>(text: &'a str, names: [&str; N]) -> Result<[&'a str; N]> {
    let mut values = [""; N];
    let mut seen = [false; N];
    for token in text.split(' ') {
        let (key, value) = token.split_once('=').context("invalid observer field")?;
        let index = names
            .iter()
            .position(|name| *name == key)
            .context("unknown observer field")?;
        ensure!(!seen[index], "duplicate observer field");
        seen[index] = true;
        values[index] = value;
    }
    ensure!(
        seen.into_iter().all(|value| value),
        "missing observer field"
    );
    Ok(values)
}

fn number<T: FromStr>(value: &str) -> Result<T> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    ensure!(
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid observer integer"
    );
    value
        .parse()
        .map_err(|_| anyhow::anyhow!("observer integer out of range"))
}

fn positive(value: &str) -> Result<i64> {
    let value = number(value)?;
    ensure!(value > 0, "nonpositive observer clock");
    Ok(value)
}

fn ordered(value: &str, previous: i64) -> Result<i64> {
    let value = positive(value)?;
    ensure!(value >= previous, "observer per-type QPC reversal");
    Ok(value)
}

fn next_id(value: u64, length: usize) -> Result<()> {
    ensure!(
        value
            == u64::try_from(length)?
                .checked_add(1)
                .context("observer ID overflow")?,
        "observer ID/sequence gap"
    );
    Ok(())
}

fn boolean(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("invalid observer boolean"),
    }
}

fn hex(value: &str) -> Result<u64> {
    let value = value
        .strip_prefix("0x")
        .context("invalid observer hex prefix")?;
    ensure!(
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid observer hexadecimal"
    );
    Ok(u64::from_str_radix(value, 16)?)
}

fn coordinates<const N: usize>(text: &str) -> Result<[i32; N]> {
    let values = text.split(',').map(number).collect::<Result<Vec<_>>>()?;
    values
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid observer coordinate count"))
}

fn rect(text: &str) -> Result<[i32; 4]> {
    let value @ [x, y, width, height] = coordinates(text)?;
    ensure!(
        width > 0
            && height > 0
            && x.checked_add(width).is_some()
            && y.checked_add(height).is_some(),
        "invalid observer rectangle"
    );
    Ok(value)
}

fn span(text: &str) -> Result<Option<[i32; 2]>> {
    if text == "-" {
        return Ok(None);
    }
    let (left, right) = text.split_once(':').context("invalid geometry span")?;
    let (left, right) = (number(left)?, number(right)?);
    ensure!(left < right, "empty or reversed geometry span");
    Ok(Some([left, right]))
}

fn inside_panel([left, top, width, height]: [i32; 4], [x, y, w, h]: [i32; 4]) -> bool {
    x >= left
        && y >= top
        && i64::from(x) + i64::from(w) <= i64::from(left) + i64::from(width)
        && i64::from(y) + i64::from(h) <= i64::from(top) + i64::from(height)
}

fn validate_union(geometry: &Geometry, previous: Option<&Geometry>) -> Result<()> {
    for (index, row) in geometry.rows.iter().enumerate() {
        let y = geometry.origin_y + i32::try_from(index)?;
        let old = previous
            .and_then(|value| value.row(y))
            .and_then(|row| row.drawn);
        let expected = match (row.drawn, old) {
            (Some([left, right]), Some([other_left, other_right]))
                if left <= other_right && other_left <= right =>
            {
                [Some([left.min(other_left), right.max(other_right)]), None]
            }
            (current, old) => [current, old],
        };
        ensure!(
            row.owned == expected,
            "geometry ownership does not match exact current/previous union"
        );
    }
    if let Some(previous) = previous {
        for (index, row) in previous.rows.iter().enumerate() {
            let y = previous.origin_y + i32::try_from(index)?;
            ensure!(
                row.drawn.is_none() || geometry.row(y).is_some(),
                "geometry omits previously drawn row"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
