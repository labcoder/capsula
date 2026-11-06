//! An ask about a clicked 3D part: one turn built from the part's own drawing text, in the
//! drawing's thread (docs/development/runs.md, "Asking about a part").

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use kumochi_protocol::message::{Content, ControlToShell, SceneId};
use kumochi_scene::{Handle, Part, Text};

use super::{Coordinator, Effect};

impl Coordinator {
    /// Counted like a typed turn, then classified once, whether or not the part resolves, so the
    /// delete fences it passed retire. A part that no longer resolves, or a thread being deleted,
    /// refuses it; otherwise its run replaces the current one and the shell is asked to follow.
    pub(super) fn asked(
        &mut self,
        turn: u32,
        scene: SceneId,
        element: Handle,
        effects: &mut Vec<Effect>,
    ) {
        if !self.count(effects) {
            return;
        }
        let part = self
            .drawing
            .part(scene, element)
            .map(|(thread, alt, part)| (thread, text(&part, alt)));
        let admitted = self.admits(part.as_ref().map(|&(thread, _)| thread));
        let Some((thread, Some(text))) = part.filter(|_| admitted) else {
            tracing::debug!(admitted, "ask about a part dropped");
            effects.push(Effect::Control(ControlToShell::TurnRefused {
                turn,
                thread: None,
            }));
            return;
        };
        self.turn(turn, Some(thread), text, effects);
        if let Some(run) = self.current_run() {
            effects.push(Effect::Control(ControlToShell::Focus { run, thread }));
        }
    }
}

/// The ask's text from `ask-about-part`, `ask-about-part-info` and `ask-about-part-alt`, one line
/// each, with the optional lines left out; `None` only if the bundled messages are broken.
pub(super) fn text(part: &Part<'_>, alt: &Text) -> Option<Content> {
    let resource =
        FluentResource::try_new(include_str!("../../../../locales/en/brain.ftl").to_owned())
            .ok()?;
    let mut bundle = FluentBundle::new(vec!["en".parse().ok()?]);
    bundle.set_use_isolating(false);
    bundle.add_resource(resource).ok()?;
    let lines = [
        Some(("ask-about-part", "label", part.label)),
        part.info.map(|info| ("ask-about-part-info", "info", info)),
        (!alt.as_str().is_empty()).then_some(("ask-about-part-alt", "alt", alt)),
    ];
    let mut out = Vec::with_capacity(lines.len());
    for (id, name, value) in lines.into_iter().flatten() {
        let pattern = bundle.get_message(id)?.value()?;
        let mut args = FluentArgs::new();
        args.set(name, value.as_str());
        let mut errors = Vec::new();
        let line = bundle.format_pattern(pattern, Some(&args), &mut errors);
        if !errors.is_empty() {
            return None;
        }
        out.push(line.into_owned());
    }
    Some(out.join("\n").into())
}
