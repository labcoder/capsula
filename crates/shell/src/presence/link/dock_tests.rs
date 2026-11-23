use super::{Batch, LinkEvent, LinkState, Presence, Session};
use crate::platform::work_area_fixture;
use crate::presence::test_fixture;
use gpui_kit::TestAppContext;
use kumochi_protocol::message::{
    AssistantName, ControlToBrain, DEFAULT_HOLD_TO_TALK, DEFAULT_SCREEN_HOLDS, DEFAULT_SHOW_SCREEN,
    DockPosition, ErrorKey, Incarnation, MaxFrameRate, OnboardingStep, SettingsOp, SettingsView,
    SttUnload, ThreadId, UserError,
};

#[gpui::test]
fn dock_refusal_replaces_its_placement_before_the_link_drops(cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let saved = DockPosition::TopCenter;
    let rejected = DockPosition::BottomRight;
    let refusal = |generation| LinkEvent::Error {
        generation,
        error: ErrorKey::SettingsRefused.into(),
    };
    let requested_position = |owner: &Presence| {
        owner
            .line
            .position_index()
            .map(|index| DockPosition::ALL[index])
    };
    for newer in [None, Some(DockPosition::MiddleLeft)] {
        let owner = test_fixture::presence(cx);
        owner.update(cx, |owner, cx| {
            owner.session = Some(Session::detached(7, Incarnation(1)));
            owner
                .link_events(
                    Batch::Plain(vec![LinkEvent::Settings {
                        generation: 7,
                        view: SettingsView {
                            assistant_name: AssistantName::new("Kumo").unwrap(),
                            user_name: None,
                            onboarded: true,
                            onboarding_from: OnboardingStep::Welcome,
                            main_thread: ThreadId(1),
                            max_frame_rate: MaxFrameRate::AUTOMATIC,
                            default_model: None,
                            dock_position: saved,
                            show_in_taskbar: false,
                            talk_back: false,
                            stt_unload: SttUnload::Kept,
                            hold_to_talk: DEFAULT_HOLD_TO_TALK,
                            screen_holds: DEFAULT_SCREEN_HOLDS,
                            show_screen: DEFAULT_SHOW_SCREEN,
                            shortcut: kumochi_protocol::message::ShortcutView::Default,
                        },
                        answer: false,
                    }]),
                    cx,
                )
                .unwrap();
            assert_eq!(requested_position(owner), Some(saved));
            let controls = owner.link.record_controls();
            for position in std::iter::once(rejected).chain(newer) {
                owner.save_dock_position(position, cx).unwrap();
                assert_eq!(
                    controls.try_recv(),
                    Ok(ControlToBrain::Settings(SettingsOp::SetDockPosition(
                        position
                    )))
                );
                assert_eq!(requested_position(owner), Some(position));
            }

            owner
                .link_events(
                    Batch::Plain(vec![
                        refusal(6),
                        LinkEvent::Error {
                            generation: 7,
                            error: ErrorKey::AccountOffline.into(),
                        },
                    ]),
                    cx,
                )
                .unwrap();
            assert_eq!(requested_position(owner), Some(newer.unwrap_or(rejected)));

            owner
                .link_events(
                    Batch::Plain(vec![refusal(7), LinkEvent::State(LinkState::Reconnecting)]),
                    cx,
                )
                .unwrap();
            assert!(owner.session.is_none());
            assert_eq!(requested_position(owner), Some(newer.unwrap_or(saved)));
            let state = owner.settings.read(cx);
            assert_eq!(state.view(), None);
            assert_eq!(state.dock_position_choice(), newer);
            assert_eq!(
                state.notice().map(UserError::key),
                Some(ErrorKey::SettingsRefused)
            );
            assert!(controls.is_empty(), "rollback isn't another save");
        });
    }
}
