use async_channel::{Receiver, TryRecvError};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AnyWindowHandle, App, Bounds, Entity, InputEvent as _, KeyDownEvent, KeyUpEvent, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, TestAppContext,
    VisualTestContext, Window, px, size,
};
use kumochi_protocol::message::{
    ConnectionKind::{self, ChatGpt, ClaudeCode},
    ControlToBrain, DataToShell, DockPosition, Effort, Incarnation, ModelChoice, ModelInfo,
    ModelName, ModelSlug, SettingsOp,
};

use super::SettingsView;
use crate::link::{Batch, LinkEvent, LinkState, Session};
use crate::presence::{Layout, LineFrame, Placement, Presence, test_fixture};

const ROWS: [&str; 3] = ["default-model-0", "default-model-1", "default-model-2"];

struct Picker {
    visual: VisualTestContext,
    view: Entity<SettingsView>,
    owner: Entity<Presence>,
    windows: [AnyWindowHandle; 4],
    controls: Receiver<ControlToBrain>,
}

impl Picker {
    fn new(cx: &mut TestAppContext) -> Self {
        let owner = test_fixture::presence(cx);
        let (window, view) = super::tests::open(cx);
        let (controls, windows) = view.update(cx, |view, cx| {
            view.owner = owner.downgrade();
            owner.update(cx, |owner, _| {
                owner.settings = view.model.clone();
                owner.line.request(Placement {
                    layout: Layout::default(),
                    frame: LineFrame::in_slot([0, 0, 100, 32], 1.0),
                    index: DockPosition::ALL
                        .iter()
                        .position(|position| *position == super::tests::saved().dock_position)
                        .expect("saved dock position"),
                });
                let text = owner.text.as_ref().expect("detached conversation");
                (
                    owner.link.record_controls(),
                    [window, text.window, owner.dock_window, owner.panel_window],
                )
            })
        });
        let visual = VisualTestContext::from_window(window, cx);
        visual.simulate_resize(size(px(768.), px(1600.)));
        let mut picker = Self {
            visual,
            view,
            owner,
            windows,
            controls,
        };
        picker.feed(vec![
            LinkEvent::Connected(Session::detached(7, Incarnation(1))),
            settings(7, ChatGpt, false),
            catalog(7, ClaudeCode, &["shared"]),
            catalog(7, ChatGpt, &["shared"]),
        ]);
        picker.draw();
        picker
    }

    fn feed(&mut self, events: Vec<LinkEvent>) {
        self.owner.update(&mut self.visual.cx, |owner, cx| {
            owner
                .link_events(Batch::Plain(events), cx)
                .expect("synthetic link frames");
        });
    }

    fn draw(&mut self) {
        self.visual.run_until_parked();
        self.visual.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn row(&mut self, index: usize) -> Bounds<Pixels> {
        self.visual
            .debug_bounds(ROWS[index])
            .expect("visible model radio")
    }

    fn assert_checked(&mut self, count: usize, index: usize) {
        let bounds: Vec<_> = (0..count).map(|index| self.row(index)).collect();
        assert!(
            ROWS.get(count)
                .is_none_or(|selector| self.visual.debug_bounds(selector).is_none())
        );
        self.visual
            .update(|window, cx| assert_checked_in_frame(window, cx, &bounds, index));
    }

    fn focus_row(&mut self, index: usize) {
        self.visual.update(|window, cx| {
            self.view.read(cx).inputs[1]
                .clone()
                .update(cx, |input, cx| input.focus(window, cx));
            for _ in 0..=index {
                window.focus_next(cx);
            }
        });
        self.draw();
    }

    fn space(&mut self) {
        self.visual.update(|window, cx| {
            space_down(window, cx);
            space_up(window, cx);
        });
    }

    fn assert_pick(&self, connection: ConnectionKind, slug: &str) {
        assert_eq!(
            self.controls.try_recv(),
            Ok(ControlToBrain::Settings(SettingsOp::SetDefaultModel(
                choice(connection, slug)
            )))
        );
        assert_eq!(self.controls.try_recv(), Err(TryRecvError::Empty));
    }

    fn close(self) {
        let Self {
            mut visual,
            owner,
            view,
            windows,
            controls,
        } = self;
        drop((owner, view, controls));
        for handle in windows {
            handle
                .update(&mut visual.cx, |_, window, _| window.remove_window())
                .expect("a fixture window");
        }
        visual.run_until_parked();
    }
}

fn choice(connection: ConnectionKind, slug: &str) -> ModelChoice {
    ModelChoice {
        connection,
        model: ModelSlug::new(slug).expect("fixture slug"),
        effort: Effort::Low,
    }
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the fixture passes through the link answer bit"
)]
fn settings(generation: u64, connection: ConnectionKind, answer: bool) -> LinkEvent {
    let mut view = super::tests::saved();
    view.default_model = Some(choice(connection, "shared"));
    LinkEvent::Settings {
        generation,
        view,
        answer,
    }
}

fn catalog(generation: u64, connection: ConnectionKind, slugs: &[&str]) -> LinkEvent {
    LinkEvent::Data {
        generation,
        message: DataToShell::Models {
            connection,
            models: slugs
                .iter()
                .map(|&slug| ModelInfo {
                    slug: ModelSlug::new(slug).expect("fixture slug"),
                    display_name: ModelName::new(slug).expect("fixture name"),
                    supported_efforts: None,
                })
                .collect(),
        },
        bytes: 0,
    }
}

fn reconnect(generation: u64) -> Vec<LinkEvent> {
    vec![
        LinkEvent::State(LinkState::Connecting),
        LinkEvent::Connected(Session::detached(generation, Incarnation(1))),
        settings(generation, ChatGpt, false),
        catalog(generation, ChatGpt, &["shared"]),
        catalog(generation, ClaudeCode, &["shared"]),
    ]
}

fn space_down(window: &mut Window, cx: &mut App) {
    let _down = window.dispatch_event(
        KeyDownEvent {
            keystroke: Keystroke::parse("space").expect("valid key"),
            is_held: false,
            prefer_character_input: false,
        }
        .to_platform_input(),
        cx,
    );
}

fn space_up(window: &mut Window, cx: &mut App) {
    let _up = window.dispatch_event(
        KeyUpEvent {
            keystroke: Keystroke::parse("space").expect("valid key"),
        }
        .to_platform_input(),
        cx,
    );
}

fn click(window: &mut Window, cx: &mut App, position: Point<Pixels>) -> bool {
    let button = MouseButton::Left;
    let modifiers = Modifiers::none();
    let _down = window.dispatch_event(
        MouseDownEvent {
            position,
            button,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }
        .to_platform_input(),
        cx,
    );
    window
        .dispatch_event(
            MouseUpEvent {
                position,
                button,
                modifiers,
                click_count: 1,
            }
            .to_platform_input(),
            cx,
        )
        .default_prevented
}

fn assert_checked_in_frame(window: &Window, cx: &App, rows: &[Bounds<Pixels>], index: usize) {
    let quads = window.painted_quads();
    let checked: Vec<_> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, bounds)| {
            let bounds = bounds.scale(window.scale_factor());
            quads
                .iter()
                .any(|quad| {
                    quad.background == cx.theme().tokens.primary.into()
                        && bounds.contains(&quad.bounds.center())
                })
                .then_some(index)
        })
        .collect();
    assert_eq!(
        checked,
        [index],
        "checked indicators in the rendered radios"
    );
}

#[gpui::test]
fn the_rendered_picker_keeps_the_right_model_through_refresh_refusal_and_reconnect(
    cx: &mut TestAppContext,
) {
    let mut picker = Picker::new(cx);
    picker.assert_checked(2, 0);
    picker.feed(vec![catalog(7, ChatGpt, &["other", "shared"])]);
    picker.draw();
    picker.assert_checked(3, 1);

    let rows = [picker.row(0), picker.row(1), picker.row(2)];
    let [_, a, b] = rows.map(|bounds| bounds.center());
    picker.visual.update(|window, cx| {
        assert!(click(window, cx, b));
        assert!(
            !click(window, cx, a),
            "the old checked radio is inert before redraw"
        );
        assert_checked_in_frame(window, cx, &rows, 1);
    });
    picker.assert_pick(ClaudeCode, "shared");
    picker.draw();
    picker.assert_checked(3, 2);

    picker.focus_row(1);
    picker.space();
    picker.assert_pick(ChatGpt, "shared");
    picker.draw();
    picker.assert_checked(3, 1);
    picker.feed(vec![settings(7, ClaudeCode, true)]);
    picker.draw();
    picker.assert_checked(3, 1);

    assert!(picker.controls.close());
    let b = picker.row(2).center();
    picker
        .visual
        .update(|window, cx| assert!(click(window, cx, b)));
    picker.draw();
    picker.assert_checked(3, 1);
    picker.visual.update(|_, cx| {
        assert!(
            !picker
                .owner
                .read(cx)
                .session
                .as_ref()
                .expect("old pair")
                .open()
        );
    });

    picker.feed(vec![LinkEvent::State(LinkState::Connecting)]);
    picker.draw();
    assert!(
        ROWS.iter()
            .all(|selector| picker.visual.debug_bounds(selector).is_none())
    );
    picker.controls = picker.owner.update(&mut picker.visual.cx, |owner, _| {
        owner.link.record_controls()
    });
    picker.feed(vec![
        LinkEvent::Connected(Session::detached(8, Incarnation(1))),
        settings(7, ChatGpt, true),
        catalog(7, ChatGpt, &["stale"]),
    ]);
    picker.draw();
    assert!(picker.visual.debug_bounds(ROWS[0]).is_none());
    assert_eq!(picker.controls.try_recv(), Err(TryRecvError::Empty));
    picker.feed(vec![
        settings(8, ClaudeCode, false),
        catalog(8, ClaudeCode, &["shared"]),
        catalog(8, ChatGpt, &["shared"]),
    ]);
    picker.draw();
    picker.assert_checked(2, 1);
    assert_eq!(picker.controls.try_recv(), Err(TryRecvError::Empty));
    picker.close();
}

#[gpui::test]
fn focused_model_should_not_change_when_a_catalog_inserts_a_row_before_it(cx: &mut TestAppContext) {
    let mut picker = Picker::new(cx);
    picker.focus_row(1);
    picker.space();
    picker.assert_pick(ClaudeCode, "shared");
    picker.feed(vec![settings(7, ChatGpt, true)]);
    picker.draw();
    let focused = picker
        .visual
        .update(|window, cx| window.focused(cx).expect("a focused radio"));
    picker.feed(vec![catalog(7, ChatGpt, &["shared", "other"])]);
    picker.draw();
    picker.assert_checked(3, 0);
    picker
        .visual
        .update(|window, _| assert!(focused.is_focused(window)));
    picker.space();
    let sent = picker.controls.try_recv();
    picker.close();
    let Ok(ControlToBrain::Settings(SettingsOp::SetDefaultModel(sent))) = sent else {
        panic!("Space did not send a model choice");
    };
    assert_eq!(
        (sent.connection, sent.model.as_str()),
        (ClaudeCode, "shared"),
        "Space should activate the same provider and model that had focus before refresh"
    );
}

#[gpui::test]
fn space_needs_new_focus_after_a_model_is_removed_or_the_link_disconnects(cx: &mut TestAppContext) {
    let mut picker = Picker::new(cx);
    picker.focus_row(1);
    picker.feed(vec![
        catalog(7, ClaudeCode, &[]),
        catalog(7, ChatGpt, &["shared", "other"]),
    ]);
    picker.draw();
    picker.assert_checked(2, 0);
    picker.space();
    assert_eq!(picker.controls.try_recv(), Err(TryRecvError::Empty));
    picker
        .visual
        .update(|window, cx| assert!(picker.view.read(cx).focus.is_focused(window)));

    picker.focus_row(1);
    picker.space();
    picker.assert_pick(ChatGpt, "other");
    picker.draw();
    picker.assert_checked(2, 1);
    picker.feed(reconnect(8));
    picker.draw();
    picker.assert_checked(2, 0);
    picker.space();
    assert_eq!(picker.controls.try_recv(), Err(TryRecvError::Empty));
    picker
        .visual
        .update(|window, cx| assert!(picker.view.read(cx).focus.is_focused(window)));

    picker.focus_row(1);
    picker.space();
    picker.assert_pick(ClaudeCode, "shared");
    picker.close();
}

#[gpui::test]
fn an_armed_space_press_cannot_cross_a_reconnect_with_the_same_model(cx: &mut TestAppContext) {
    let mut picker = Picker::new(cx);
    picker.focus_row(1);
    picker.visual.update(|window, cx| {
        space_down(window, cx);
        picker.owner.update(cx, |owner, cx| {
            owner
                .link_events(Batch::Plain(reconnect(8)), cx)
                .expect("synthetic reconnect");
        });
        space_up(window, cx);
    });
    assert_eq!(picker.controls.try_recv(), Err(TryRecvError::Empty));

    picker.draw();
    picker.focus_row(1);
    picker.visual.update(space_down);
    picker.feed(reconnect(9));
    picker.draw();
    picker.visual.update(space_up);
    assert_eq!(picker.controls.try_recv(), Err(TryRecvError::Empty));
    picker
        .visual
        .update(|window, cx| assert!(picker.view.read(cx).focus.is_focused(window)));

    picker.focus_row(1);
    picker.space();
    picker.assert_pick(ClaudeCode, "shared");
    picker.close();
}
