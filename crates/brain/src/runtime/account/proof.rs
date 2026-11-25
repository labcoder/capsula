//! Which history the current `ChatGPT` sign-in may read, asked of the vault once per connection.
//! Until the vault answers, and whenever it can't tell, the account isn't ready.

use std::sync::mpsc as std_mpsc;

use tokio::sync::{mpsc, oneshot};

use super::{Event, Work};
use crate::connections::vault::Proof;

/// One connection's check, under the epoch that tells it from an earlier check of the same tag.
#[derive(Clone, Copy, Debug)]
struct Check {
    connection: u64,
    epoch: u64,
    sent: bool,
    proof: Option<Proof>,
    /// A catalog read waits for this check.
    read_owed: bool,
}

/// The current connection's check. Checks wait while the vault's history link waits for its
/// writer, so a healthy replacement isn't found unproved only because startup asked first.
#[derive(Debug)]
pub(super) struct Proofs {
    check: Option<Check>,
    epochs: u64,
    held: bool,
    /// Shutdown began: no check goes out, and no answer starts a catalog read.
    closing: bool,
}

impl Proofs {
    pub(super) fn held() -> Self {
        Self {
            check: None,
            epochs: 0,
            held: true,
            closing: false,
        }
    }

    pub(super) fn close(&mut self) {
        self.closing = true;
        self.held = true;
        if let Some(check) = &mut self.check {
            check.read_owed = false;
        }
    }

    /// Checks may go from now on; one already due goes at once.
    pub(super) fn release(
        &mut self,
        jobs: Option<&std_mpsc::Sender<Work>>,
        reads: &mpsc::UnboundedSender<Event>,
    ) {
        self.held = self.closing;
        self.send(jobs, reads);
    }

    /// The account's connection is now `connection`: a different one is checked afresh, and
    /// nothing found for the one before applies to it.
    pub(super) fn track(
        &mut self,
        connection: Option<u64>,
        jobs: Option<&std_mpsc::Sender<Work>>,
        reads: &mpsc::UnboundedSender<Event>,
    ) {
        if self.check.map(|check| check.connection) == connection {
            return;
        }
        self.check = connection.map(|connection| {
            self.epochs += 1;
            Check {
                connection,
                epoch: self.epochs,
                sent: false,
                proof: None,
                read_owed: false,
            }
        });
        self.send(jobs, reads);
    }

    fn send(
        &mut self,
        jobs: Option<&std_mpsc::Sender<Work>>,
        reads: &mpsc::UnboundedSender<Event>,
    ) {
        if self.held {
            return;
        }
        let Some(check) = self.check.as_mut().filter(|check| !check.sent) else {
            return;
        };
        check.sent = true;
        let (connection, epoch) = (check.connection, check.epoch);
        let (reply, answer) = oneshot::channel();
        // A job that can't be queued drops its reply, which answers below.
        if let Some(jobs) = jobs {
            jobs.send(Work::Proof { connection, reply }).ok();
        }
        let reads = reads.clone();
        drop(tokio::spawn(async move {
            // A job the credential thread dropped, or never ran, proves nothing.
            let proof = answer.await.unwrap_or(Proof::Unproved);
            reads
                .send(Event::Proof {
                    connection,
                    epoch,
                    proof,
                })
                .ok();
        }));
    }

    /// Whether an answer for `connection` under `epoch` is the one the current check waits for.
    pub(super) fn awaits(&self, connection: u64, epoch: u64) -> bool {
        self.check.is_some_and(|check| {
            check.connection == connection && check.epoch == epoch && check.proof.is_none()
        })
    }

    /// The current check, unanswered, holds back a catalog read.
    pub(super) fn owe_read(&mut self) {
        if self.closing {
            return;
        }
        if let Some(check) = self.check.as_mut().filter(|check| check.proof.is_none()) {
            check.read_owed = true;
        }
    }

    /// Records the current check's answer, and says whether a catalog read waited for it; any
    /// other answer changes nothing. A cutoff for another connection proves nothing.
    // shortcut: an unproved connection is never checked again until the connection changes,
    // even when only a transient authority read made it unproved; repair ingress must check
    // again once the replacement's context is established.
    pub(super) fn found(&mut self, connection: u64, epoch: u64, proof: Proof) -> Option<bool> {
        if !self.awaits(connection, epoch) {
            return None;
        }
        let proof = match proof {
            Proof::Recovered(cut) if cut.tag() != connection => Proof::Unproved,
            proof => proof,
        };
        let check = self.check.as_mut()?;
        check.proof = Some(proof);
        Some(std::mem::take(&mut check.read_owed))
    }

    /// What `connection`'s check found, when it's the current check and has answered.
    pub(super) fn proof(&self, connection: u64) -> Option<Proof> {
        self.check
            .filter(|check| check.connection == connection)
            .and_then(|check| check.proof)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;

    use tokio::sync::mpsc;

    use super::{Event, Proofs, Work};
    use crate::connections::vault::Proof;
    use crate::store::Boundary;

    fn reads() -> (mpsc::UnboundedSender<Event>, mpsc::UnboundedReceiver<Event>) {
        mpsc::unbounded_channel()
    }

    /// The next queued check's connection and reply.
    fn queued(
        jobs: &std_mpsc::Receiver<Work>,
    ) -> Option<(u64, tokio::sync::oneshot::Sender<Proof>)> {
        match jobs.try_recv().ok()? {
            Work::Proof { connection, reply } => Some((connection, reply)),
            _ => None,
        }
    }

    /// The freshness hold: an answer counts only for the check it was asked under. One for a
    /// connection never held, or for the same tag before a sign-out gap, changes nothing, and
    /// the current check still waits.
    #[test]
    fn an_answer_counts_only_for_the_current_connection_and_epoch() {
        let (reads, _answers) = reads();
        let mut proofs = Proofs::held();
        proofs.track(Some(7), None, &reads);
        assert!(proofs.awaits(7, 1));
        proofs.track(None, None, &reads);
        assert!(!proofs.awaits(7, 1), "the sign-out gap");
        assert_eq!(proofs.found(7, 1, Proof::Ordinary), None);
        proofs.track(Some(7), None, &reads);
        assert_eq!(
            proofs.found(7, 1, Proof::Ordinary),
            None,
            "same tag, older epoch"
        );
        assert_eq!(
            proofs.found(99, 2, Proof::Ordinary),
            None,
            "a connection never held"
        );
        assert_eq!(proofs.proof(7), None);
        assert!(proofs.awaits(7, 2));
        assert_eq!(proofs.found(7, 2, Proof::Ordinary), Some(false));
        assert_eq!(proofs.proof(7), Some(Proof::Ordinary));
        assert_eq!(proofs.found(7, 2, Proof::Unproved), None, "answered once");
        assert_eq!(proofs.proof(7), Some(Proof::Ordinary));
    }

    /// The same connection again, as a refresh leaves it, is never checked again.
    #[test]
    fn the_same_connection_keeps_its_answer() {
        let (reads, _answers) = reads();
        let mut proofs = Proofs::held();
        proofs.track(Some(7), None, &reads);
        let cut = Boundary::fixture(7, 3, 9);
        assert_eq!(proofs.found(7, 1, Proof::Recovered(cut)), Some(false));
        proofs.track(Some(7), None, &reads);
        assert_eq!(proofs.proof(7), Some(Proof::Recovered(cut)));
    }

    #[test]
    fn a_cutoff_for_another_connection_proves_nothing() {
        let (reads, _answers) = reads();
        let mut proofs = Proofs::held();
        proofs.track(Some(7), None, &reads);
        proofs.owe_read();
        assert_eq!(
            proofs.found(7, 1, Proof::Recovered(Boundary::fixture(8, 3, 9))),
            Some(true)
        );
        assert_eq!(proofs.proof(7), Some(Proof::Unproved));
    }

    /// Checks wait for the history writer, then the one due goes, once.
    #[tokio::test]
    async fn a_check_waits_for_the_writer_and_goes_once() {
        let (reads, mut answers) = reads();
        let (jobs, queue) = std_mpsc::channel();
        let mut proofs = Proofs::held();
        proofs.track(Some(7), Some(&jobs), &reads);
        assert!(queued(&queue).is_none(), "held");
        proofs.release(Some(&jobs), &reads);
        let (connection, reply) = queued(&queue).expect("the due check goes");
        assert_eq!(connection, 7);
        proofs.release(Some(&jobs), &reads);
        proofs.track(Some(7), Some(&jobs), &reads);
        assert!(queued(&queue).is_none(), "once");
        reply.send(Proof::Ordinary).expect("waiting");
        let Some(Event::Proof {
            connection: 7,
            epoch: 1,
            proof: Proof::Ordinary,
        }) = answers.recv().await
        else {
            panic!("the answer comes back under its epoch");
        };
    }

    /// A check whose job the credential thread drops, or can never take, ends unproved.
    #[tokio::test]
    async fn a_lost_check_is_unproved() {
        let (reads, mut answers) = reads();
        let (jobs, queue) = std_mpsc::channel();
        let mut proofs = Proofs::held();
        proofs.release(Some(&jobs), &reads);
        proofs.track(Some(7), Some(&jobs), &reads);
        drop(queued(&queue).expect("queued"));
        drop(queue);
        proofs.track(Some(8), Some(&jobs), &reads);
        let mut lost = Vec::new();
        for _ in 0..2 {
            let Some(Event::Proof {
                connection,
                epoch,
                proof: Proof::Unproved,
            }) = answers.recv().await
            else {
                panic!("a lost check answers unproved");
            };
            lost.push((connection, epoch));
        }
        lost.sort_unstable();
        assert_eq!(lost, [(7, 1), (8, 2)]);
    }

    /// An answer that arrives after shutdown began starts no catalog read, and nothing new is owed.
    #[test]
    fn an_answer_during_close_starts_no_catalog_read() {
        let (reads, _answers) = reads();
        let mut proofs = Proofs::held();
        proofs.track(Some(7), None, &reads);
        proofs.owe_read();
        proofs.close();
        proofs.owe_read();
        assert_eq!(proofs.found(7, 1, Proof::Ordinary), Some(false));
    }
}
