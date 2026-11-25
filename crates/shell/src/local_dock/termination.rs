//! macOS termination negotiation: a quit `AppKit` starts on its own, such as from the Dock or at
//! logout, is answered by the dock's custody.

use std::rc::Rc;

use gpui_macos::{MacPlatform, TerminationRequest};

use super::Custody;

/// The graphical application whose termination requests `custody` answers.
pub(super) fn application(custody: &Rc<Custody>) -> gpui_kit::Application {
    let answers = Rc::clone(custody);
    let platform = MacPlatform::new_with_termination_handler(
        false,
        Box::new(move |request: TerminationRequest| {
            answers.answer_termination(move |allow| request.reply(allow));
        }),
    );
    // GPUI's own quit sends terminate: from a main-queue block, where a Later reply queued behind
    // that block can never arrive; the platform's quit sends it only when it answers Now.
    if let Some(app_quit) = platform.app_quit() {
        custody.quit_through(move || app_quit.quit());
    }
    gpui_kit::Application::with_platform(Rc::new(platform))
}
