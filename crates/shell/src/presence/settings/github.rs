//! GitHub account controls over the Settings model's current-session snapshot.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::base::Link;
use gpui_kit::component::button::Button;
use gpui_kit::component::select::Select;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IndexPath};
use gpui_kit::{
    App, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Styled as _, Window, div,
};
use kumochi_protocol::message::{GitHubOp, GitHubView, RepoName, VERIFICATION_URI};

use super::super::Presence;
use super::{SettingsTab, SettingsView};
use crate::link::Session;
use crate::strings::{Strings, Text};

#[derive(Clone)]
pub(super) struct RepoChoice {
    repo: RepoName,
    generation: Option<u64>,
    origin: Arc<()>,
}

impl PartialEq for RepoChoice {
    fn eq(&self, other: &Self) -> bool {
        self.repo == other.repo
            && self.generation == other.generation
            && Arc::ptr_eq(&self.origin, &other.origin)
    }
}

impl std::fmt::Debug for RepoChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitHubRepositoryChoice")
    }
}

impl gpui_kit::component::select::SelectItem for RepoChoice {
    type Value = Self;

    fn title(&self) -> SharedString {
        self.repo.as_str().to_owned().into()
    }

    fn value(&self) -> &Self::Value {
        self
    }
}

#[cfg(debug_assertions)]
fn configured(view: &SettingsView) -> bool {
    #[cfg(test)]
    if let Some(configured) = view.github_configured {
        return configured;
    }
    kumochi_protocol::constants::GITHUB_CLIENT_ID.is_some() || view.fixture
}

#[cfg(not(debug_assertions))]
fn configured(_: &SettingsView) -> bool {
    kumochi_protocol::constants::GITHUB_CLIENT_ID.is_some()
}

impl SettingsView {
    pub(super) fn github_generation(&self, cx: &App) -> Option<u64> {
        self.owner
            .upgrade()?
            .read(cx)
            .session
            .as_ref()
            .map(Session::generation)
    }

    pub(super) fn sync_github(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let state = &self.model.read(cx).github;
        let mut selected = None;
        let items = match &state.view {
            Some(GitHubView::Connected { repos, default, .. }) => repos
                .iter()
                .enumerate()
                .map(|(ix, repo)| {
                    if default.as_ref() == Some(repo) {
                        selected = Some(IndexPath::new(ix));
                    }
                    RepoChoice {
                        repo: repo.clone(),
                        generation: state.generation,
                        origin: Arc::clone(&state.controls_origin),
                    }
                })
                .collect(),
            None
            | Some(
                GitHubView::SignedOut { .. }
                | GitHubView::Requesting
                | GitHubView::AwaitingUser { .. }
                | GitHubView::ConnectedUnlisted { .. }
                | GitHubView::NotInstalled { .. }
                | GitHubView::NeedsSignIn { .. }
                | GitHubView::NotVerified { .. },
            ) => Vec::new(),
        };
        let expires = state
            .code(state.generation, Instant::now())
            .map(|(_, seconds)| seconds);
        self.github_repos.update(cx, |picker, cx| {
            picker.set_items(items, window, cx);
            picker.set_selected_index(selected, window, cx);
        });
        drop(self.github_expiry.take());
        self.github_expiry = expires.map(|seconds| {
            cx.spawn(async move |view, cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(seconds))
                    .await;
                let _gone = view.update(cx, |_, cx| cx.notify());
            })
        });
    }

    pub(super) fn schedule_github_on_open(&self, cx: &mut Context<'_, Self>) {
        let state = &self.model.read(cx).github;
        if !state.list_on_accounts_visit(state.generation) {
            return;
        }
        let owner = self.owner.clone();
        cx.defer(move |cx| {
            let _gone = owner.update(cx, Presence::refresh_github_on_accounts_open);
        });
    }

    pub(super) fn accounts_opened(&self, generation: Option<u64>, cx: &mut Context<'_, Self>) {
        let available = self.tab == SettingsTab::Accounts && configured(self);
        #[cfg(debug_assertions)]
        let available = available && !self.fixture;
        self.model.update(cx, |model, _| {
            if available {
                model.github.open_accounts_visit(generation);
            } else {
                model.github.close_accounts_visit();
            }
        });
        self.schedule_github_on_open(cx);
    }

    pub(super) fn choose_github_repo(
        &mut self,
        choice: &RepoChoice,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let current = self.github_generation(cx);
        if self
            .model
            .read(cx)
            .github
            .is_current(current, choice.generation, &choice.origin)
        {
            let _gone = self.owner.update(cx, |owner, cx| {
                owner.github_op(GitHubOp::SetDefaultRepo(choice.repo.clone()), cx);
            });
        }
        self.sync_github(window, cx);
    }

    fn github_button(&self, label: Text, op: &GitHubOp, cx: &Context<'_, Self>) -> Button {
        let id = match op {
            GitHubOp::Connect => "github-connect",
            GitHubOp::CancelConnect => "github-cancel",
            GitHubOp::RetryCleanup => "github-retry-cleanup",
            GitHubOp::RefreshRepos => "github-refresh",
            GitHubOp::SignOut => "github-sign-out",
            GitHubOp::SetDefaultRepo(_) => unreachable!("repository selection uses Select"),
            GitHubOp::ListReposIfUnlisted => unreachable!("opening Accounts loads a missing list"),
        };
        let state = &self.model.read(cx).github;
        let generation = state.generation;
        let origin = Arc::clone(&state.controls_origin);
        let configured = configured(self) || *op != GitHubOp::Connect;
        let allowed = configured && state.allows(self.github_generation(cx), op);
        let owner = self.owner.clone();
        let op = op.clone();
        Button::new(id)
            .debug_selector(move || id.into())
            .label(cx.global::<Strings>().get(label))
            .disabled(!allowed)
            .on_click(move |_, _, cx| {
                if !configured {
                    return;
                }
                let _gone = owner.update(cx, |owner, cx| {
                    let current = owner.session.as_ref().map(Session::generation);
                    if owner
                        .settings
                        .read(cx)
                        .github
                        .is_current(current, generation, &origin)
                    {
                        owner.github_op(op.clone(), cx);
                    }
                });
            })
    }

    fn github_verification(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let state = &self.model.read(cx).github;
        let code = state
            .code(self.github_generation(cx), Instant::now())
            .map(|(code, _)| code.clone());
        let model = self.model.clone();
        let owner = self.owner.clone();
        let generation = state.generation;
        let origin = Arc::clone(&state.controls_origin);
        #[cfg(debug_assertions)]
        let fixture = self.fixture;
        let strings = cx.global::<Strings>();
        let mut block = div().flex().flex_col().gap_2();
        if let Some(code) = code {
            block = block
                .child(
                    div()
                        .debug_selector(|| "github-code".into())
                        .text_lg()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(code.as_str().to_owned()),
                )
                .child(
                    Link::new("github-verification")
                        .debug_selector(|| "github-verification".into())
                        .href(VERIFICATION_URI)
                        .accessibility_label(strings.get(Text::GitHubVerification))
                        .text_color(cx.theme().link)
                        .underline()
                        .open_with(move |_, _, _, cx| {
                            let Some(owner) = owner.upgrade() else {
                                return;
                            };
                            let current = owner.read(cx).session.as_ref().map(Session::generation);
                            let state = &model.read(cx).github;
                            if !state.is_current(current, generation, &origin)
                                || state
                                    .code(current, Instant::now())
                                    .map(|(current, _)| current)
                                    != Some(&code)
                            {
                                return;
                            }
                            #[cfg(debug_assertions)]
                            if fixture {
                                return;
                            }
                            cx.open_url(VERIFICATION_URI);
                        })
                        .child(strings.get(Text::GitHubVerification)),
                );
        } else {
            block = block.child(
                div()
                    .debug_selector(|| "github-code-expired".into())
                    .text_sm()
                    .child(strings.get(Text::GitHubCodeExpired)),
            );
        }
        block
    }

    fn github_repositories(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let current = self.github_generation(cx);
        let state = &self.model.read(cx).github;
        let Some(GitHubView::Connected {
            repos, truncated, ..
        }) = &state.view
        else {
            return div();
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .min_w_0()
            .child(
                div()
                    .text_sm()
                    .child(strings.get(Text::GitHubDefaultRepository)),
            )
            .child(
                div()
                    .debug_selector(|| "github-repository".into())
                    .min_w_0()
                    .child(
                        Select::new(&self.github_repos)
                            .id("github-repository")
                            .w_full()
                            .accessibility_label(strings.get(Text::GitHubDefaultRepository))
                            .placeholder(strings.get(Text::GitHubChooseRepository))
                            .disabled(
                                repos.is_empty()
                                    || *truncated
                                    || !state.allows(current, &GitHubOp::RefreshRepos),
                            ),
                    ),
            )
            .children((repos.is_empty() || *truncated).then(|| {
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(if *truncated {
                        Text::GitHubTruncatedRepositories
                    } else {
                        Text::GitHubEmptyRepositories
                    }))
            }))
    }

    pub(super) fn github(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let strings = cx.global::<Strings>();
        let state = &self.model.read(cx).github;
        let configured = configured(self);
        let current = self.github_generation(cx);
        let status = if !configured && state.view.is_none() {
            Text::GitHubUnavailable
        } else if current.is_none() || current != state.generation {
            Text::GitHubWaiting
        } else {
            state.status()
        };
        let mut block = div()
            .id("github-accounts")
            .debug_selector(|| "github-accounts".into())
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .pt_4()
            .child(div().text_lg().child(strings.get(Text::GitHubProvider)))
            .child(
                div()
                    .debug_selector(|| "github-status".into())
                    .text_sm()
                    .child(strings.get(status)),
            );
        if !configured {
            block = block.child(
                div()
                    .debug_selector(|| "github-configuration".into())
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::GitHubConfiguration)),
            );
        }
        if let Some(login) = state.login() {
            block = block.child(
                div()
                    .debug_selector(|| "github-login".into())
                    .text_sm()
                    .child(login.as_str().to_owned()),
            );
        }
        if matches!(state.view, Some(GitHubView::AwaitingUser { .. }))
            && status != Text::GitHubCanceling
        {
            block = block.child(self.github_verification(cx));
        }
        if matches!(state.view, Some(GitHubView::Connected { .. })) {
            block = block.child(self.github_repositories(cx));
        }
        if matches!(
            state.view,
            Some(GitHubView::ConnectedUnlisted { listing: false, .. })
        ) && status == Text::GitHubConnected
        {
            block = block.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(strings.get(Text::GitHubUnlistedRepositories)),
            );
        }
        if let Some(error) = state.error_text() {
            block = block.child(
                div()
                    .debug_selector(|| "github-error".into())
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.get(error)),
            );
        }
        if state.send_failed {
            block = block.child(
                div()
                    .debug_selector(|| "github-send-failed".into())
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(strings.get(Text::AccountSendFailed)),
            );
        }
        block.child(
            div().flex().flex_wrap().gap_2().children(
                state
                    .commands()
                    .iter()
                    .map(|(label, op)| self.github_button(*label, op, cx)),
            ),
        )
    }
}

impl Presence {
    fn refresh_github_on_accounts_open(&mut self, cx: &mut Context<'_, Self>) {
        let current = self.session.as_ref().map(Session::generation);
        if self.settings_open()
            && self
                .settings
                .read(cx)
                .github
                .list_on_accounts_visit(current)
        {
            self.github_op(GitHubOp::ListReposIfUnlisted, cx);
        }
    }

    pub(in crate::presence) fn settle_github_interactive(&mut self, cx: &mut Context<'_, Self>) {
        let current = self.session.as_ref().map(Session::generation);
        let state = &self.settings.read(cx).github;
        let generation = state.generation;
        let terminal = state.error.is_some()
            || (!matches!(
                state.view,
                Some(GitHubView::Requesting | GitHubView::AwaitingUser { .. })
            ) && state.status() != Text::GitHubRequesting);
        if let Some(panel) = &self.settings_panel {
            panel.view.update(cx, |view, _| {
                if view.github_interactive.is_some_and(|attempt| {
                    current != Some(attempt) || generation != Some(attempt) || terminal
                }) {
                    view.github_interactive = None;
                }
            });
        }
    }

    pub(in crate::presence) fn github_op(
        &mut self,
        op: GitHubOp,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let start = op == GitHubOp::Connect;
        let stop = matches!(op, GitHubOp::CancelConnect | GitHubOp::SignOut);
        let (link, session) = (&self.link, &self.session);
        let generation = session.as_ref().map(Session::generation);
        let sent = self.settings.update(cx, |state, cx| {
            let sent = state.github.send(generation, op, |op| {
                session.as_ref().is_some_and(|session| {
                    link.send_control(
                        session,
                        kumochi_protocol::message::ControlToBrain::GitHub(op),
                    )
                })
            });
            cx.notify();
            sent
        });
        if sent
            && (start || stop)
            && let Some(panel) = &self.settings_panel
        {
            panel.view.update(cx, |view, _| {
                if start {
                    view.github_interactive = generation;
                } else if stop {
                    view.github_interactive = None;
                }
            });
        }
        sent
    }

    pub(in crate::presence) fn dismiss_github_settings(&mut self, cx: &mut Context<'_, Self>) {
        self.settings
            .update(cx, |model, _| model.github.close_accounts_visit());
        let interactive = self.settings_panel.as_ref().and_then(|panel| {
            panel
                .view
                .update(cx, |view, _| view.github_interactive.take())
        });
        if interactive.is_none() || interactive != self.session.as_ref().map(Session::generation) {
            return;
        }
        if !self
            .settings
            .read(cx)
            .github
            .allows(interactive, &GitHubOp::CancelConnect)
        {
            return;
        }
        if !self.github_op(GitHubOp::CancelConnect, cx) {
            self.settings.update(cx, |state, cx| {
                state.github.send_failed = true;
                cx.notify();
            });
            // A hidden sign-in must stop; ending its stalled link also interrupts any active run.
            if let Some(session) = &self.session {
                self.link.end(session);
            }
        }
    }
}

#[cfg(test)]
mod tests;
