use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
};

use tokio::sync::Notify;
use tokio::time::{Instant, timeout_at};

use crate::config::tuning::GitHubTuning;
use crate::connections::clock::Clock;
use crate::connections::github::GitHubEndpoint;
use crate::connections::http_client::{Endpoint, Http, HttpError, Request, Response, SharedHttp};

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

const STOPPED: u8 = 1;
const HANDED: u8 = 2;

pub(in crate::runtime) struct Control {
    tuning: GitHubTuning,
    deadline: Instant,
    scope: crate::issues::WriteScope,
    state: AtomicU8,
    requests: AtomicUsize,
    server_date: AtomicU64,
    stopped: Notify,
}

impl Control {
    pub(in crate::runtime) fn new(tuning: GitHubTuning) -> Arc<Self> {
        let deadline = Instant::now() + tuning.filing_deadline;
        Arc::new(Self {
            tuning,
            deadline,
            scope: crate::issues::WriteScope::new(deadline),
            state: AtomicU8::new(0),
            requests: AtomicUsize::new(0),
            server_date: AtomicU64::new(0),
            stopped: Notify::new(),
        })
    }

    pub(in crate::runtime) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(in crate::runtime) fn cancel(&self) -> bool {
        let before = self.state.fetch_or(STOPPED, Ordering::AcqRel);
        self.scope.cancel();
        self.stopped.notify_waiters();
        before & HANDED != 0
    }

    pub(super) fn scope(&self) -> &crate::issues::WriteScope {
        &self.scope
    }

    pub(in crate::runtime) fn cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) & STOPPED != 0
    }

    pub(in crate::runtime) fn may_have_sent(&self) -> bool {
        self.state.load(Ordering::Acquire) & HANDED != 0
    }

    pub(in crate::runtime) fn server_date_s(&self) -> Option<u64> {
        match self.server_date.load(Ordering::Acquire) {
            0 => None,
            date => Some(date),
        }
    }
}

impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssueTransportControl")
            .field("cancelled", &self.cancelled())
            .field("may_have_sent", &self.may_have_sent())
            .field("requests", &self.requests.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

struct PostAccess {
    until: Instant,
    expires_at_s: u64,
    clock: Arc<dyn Clock>,
}

pub(in crate::runtime) struct BoundedHttp {
    inner: SharedHttp,
    control: Arc<Control>,
    post_access: Option<PostAccess>,
}

impl BoundedHttp {
    pub(in crate::runtime) fn new(inner: SharedHttp, control: Arc<Control>) -> Self {
        Self {
            inner,
            control,
            post_access: None,
        }
    }

    pub(in crate::runtime) fn tuning(&self) -> GitHubTuning {
        self.control.tuning
    }

    pub(in crate::runtime) fn with_post_access(
        mut self,
        until: Instant,
        expires_at_s: u64,
        clock: Arc<dyn Clock>,
    ) -> Self {
        self.post_access = Some(PostAccess {
            until,
            expires_at_s,
            clock,
        });
        self
    }
}

impl fmt::Debug for BoundedHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedIssueHttp")
            .field("control", &self.control)
            .field("post_access", &self.post_access.is_some())
            .finish_non_exhaustive()
    }
}

impl Http for BoundedHttp {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        let post = match &request.endpoint {
            Endpoint::GitHub(GitHubEndpoint::CreateIssue { .. }) => true,
            Endpoint::GitHub(
                GitHubEndpoint::User { .. }
                | GitHubEndpoint::Installations { .. }
                | GitHubEndpoint::Repositories { .. }
                | GitHubEndpoint::Issues { .. },
            ) => false,
            _ => return Err(HttpError::Request),
        };
        let stopped = self.control.stopped.notified();
        tokio::pin!(stopped);
        let _enabled = stopped.as_mut().enable();
        if self.control.cancelled() {
            return Err(HttpError::Request);
        }
        if Instant::now() >= self.control.deadline() {
            self.control.cancel();
            return Err(if post {
                HttpError::Request
            } else {
                HttpError::Timeout
            });
        }
        self.control
            .requests
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.control.tuning.filing_requests).then_some(count + 1)
            })
            .map_err(|_| HttpError::Request)?;
        if post {
            let access = self.post_access.as_ref().ok_or(HttpError::Request)?;
            let wall = access.clock.unix_seconds();
            let now = Instant::now();
            if now >= access.until
                || wall.is_none_or(|wall| wall >= access.expires_at_s)
                || now >= self.control.deadline()
            {
                self.control.cancel();
                return Err(HttpError::Request);
            }
            self.control
                .state
                .compare_exchange(0, HANDED, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| HttpError::Request)?;
        } else if self.control.cancelled() {
            return Err(HttpError::Request);
        } else if Instant::now() >= self.control.deadline() {
            self.control.cancel();
            return Err(HttpError::Timeout);
        }
        let response = tokio::select! {
            biased;
            () = &mut stopped => {
                return Err(if post { HttpError::Timeout } else { HttpError::Request });
            }
            result = timeout_at(self.control.deadline(), self.inner.execute(request)) => {
                let Ok(response) = result else {
                    self.control.cancel();
                    return Err(HttpError::Timeout);
                };
                response?
            }
        };
        if !post && let Some(date) = response.meta.date_s.filter(|date| *date > 0) {
            let _ = self.control.server_date.compare_exchange(
                0,
                date,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        Ok(response)
    }
}
