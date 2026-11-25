//! Local startup classification shared by Shell and Brain. No I/O or wire representation.

/// A read under the cooperating product writers' guard. Missing is a definitive result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    /// No object at the expected product location.
    Absent,
    /// The actual object matches the expected binding and role.
    Matching,
    /// Inaccessible, unsettled, damaged, or bound to something else.
    Unverified,
}

/// Setup writes, in publication order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Reciprocal installation owner in the product's credential store.
    Owner,
    /// Account-session lock file.
    Lock,
    /// Credential authority database.
    Authority,
    /// Committed local host, not a provider account.
    Host,
    /// History database.
    History,
    /// Final publication of the filesystem binding.
    Publish,
}

/// Durable status of the next write, not inferred from the target's disappearance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// The write was definitely not issued.
    Unstarted,
    /// The intent was recorded before attempting the write.
    Issued,
}

/// Observation of the protected new-format root and its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    /// Both root and record are definitively absent.
    Absent,
    /// Identifiable operation, with a verified record for this root.
    Pending(Step, Intent),
    /// The final record matches this root.
    Ready,
    /// Includes a bare directory without an identifiable operation.
    Unverified,
}

/// Provenance of one prospective allocation target, independent of whether old data was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    /// A product-owned target disjoint from existing allocations is established.
    VerifiedDisjoint,
    /// Unproved, including a target known only to be absent.
    Unverified,
}

/// Product-local observations of one edition in the same guard interval. No identifiers or secrets.
#[derive(Clone, Copy, Debug)]
pub struct ProfileFacts {
    /// All known legacy footprints absent, a validated intact legacy installation, or neither.
    pub legacy: Observation,
    /// The new-format allocation slot.
    pub root: Root,
    /// The exact edition-scoped OS-store owner record.
    pub owner: Observation,
    /// Lock belonging to the new allocation.
    pub lock: Observation,
    /// Binding read inside the actual opened new-format authority database.
    pub authority: Observation,
    /// Existing host for that authority.
    pub host: Observation,
    /// Binding read inside the actual opened new-format history database.
    pub history: Observation,
    /// Positive evidence for a new protected root.
    pub fresh_root: Destination,
    /// Positive evidence for a fresh, reserved credential namespace.
    pub fresh_namespace: Destination,
}

/// What could be observed before startup.
#[derive(Clone, Copy, Debug)]
pub enum Inspection {
    /// A cooperating owner holds the guard.
    Busy,
    /// The guard or user/edition context could not be established.
    Unavailable,
    /// Snapshot for one guarded inspection.
    Held(ProfileFacts),
}

/// What normal first open can consider next. No variant authorizes a write or provider sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Eligible for background first-use setup if the live allocator revalidates and reserves both targets.
    FirstUse,
    /// Reopen without creating, subject to independent admission by the actual owner.
    OpenExisting,
    /// Reinspect the pending setup only. Even an unstarted owner reservation must revalidate
    /// the credential namespace under the reacquired guard before any write.
    ResumeSetup,
    /// Another session is active.
    Busy,
    /// Preserve the data; offer retry, a fixed diagnostic code, or quit.
    Blocked,
}

impl Inspection {
    /// Classifies a single inspection. A possibly issued write without its precise result stays blocked.
    #[must_use]
    pub fn classify(self) -> Decision {
        use Observation::{Absent, Matching, Unverified};

        let facts = match self {
            Self::Busy => return Decision::Busy,
            Self::Unavailable => return Decision::Blocked,
            Self::Held(facts) => facts,
        };
        let objects = [
            (Step::Owner, facts.owner),
            (Step::Lock, facts.lock),
            (Step::Authority, facts.authority),
            (Step::Host, facts.host),
            (Step::History, facts.history),
        ];
        match (facts.legacy, facts.root) {
            (legacy, Root::Absent) if objects.iter().all(|(_, item)| *item == Absent) => {
                match legacy {
                    Matching => Decision::OpenExisting,
                    Absent
                        if facts.fresh_root == Destination::VerifiedDisjoint
                            && facts.fresh_namespace == Destination::VerifiedDisjoint =>
                    {
                        Decision::FirstUse
                    }
                    Absent | Unverified => Decision::Blocked,
                }
            }
            (Absent, Root::Ready) if objects.iter().all(|(_, item)| *item == Matching) => {
                Decision::OpenExisting
            }
            (Absent, Root::Pending(step, intent))
                if (step, intent) != (Step::Publish, Intent::Issued)
                    && objects.iter().all(|&(target, item)| {
                        if target < step || (target == step && intent == Intent::Issued) {
                            item == Matching
                        } else {
                            item == Absent
                        }
                    }) =>
            {
                Decision::ResumeSetup
            }
            _ => Decision::Blocked,
        }
    }
}

/// The outcome of trying the cooperating writers' setup guard.
#[derive(Debug)]
pub enum Guard<G> {
    /// Taken; the guard stays held while the value lives.
    Held(G),
    /// A cooperating owner holds it.
    Busy,
    /// The guard or its user and edition context could not be established.
    Unavailable,
}

/// Read-only probes of one edition's footprints. Each looks under the held guard and creates,
/// changes and removes nothing.
pub trait Probes {
    /// The held setup guard.
    type Guard;

    /// Tries the setup guard without waiting.
    fn guard(&self) -> Guard<Self::Guard>;

    /// Legacy footprints: all definitively absent, a validated intact installation, or neither.
    fn legacy(&self, guard: &Self::Guard) -> Observation;

    /// The new-format root slot and its record.
    fn root(&self, guard: &Self::Guard) -> Root;

    /// The exact edition-scoped owner record in the OS credential store.
    fn owner(&self, guard: &Self::Guard) -> Observation;
}

/// Takes the guard, then samples every fact under it, and releases it on return. Objects inside
/// the new-format root are absent only with the root itself; nothing yet reads inside one. Fresh
/// targets stay unverified until an allocator can prove them disjoint, so this never yields
/// first use, and like `classify` it authorizes nothing.
pub fn observe<P: Probes>(probes: &P) -> Inspection {
    let guard = match probes.guard() {
        Guard::Held(guard) => guard,
        Guard::Busy => return Inspection::Busy,
        Guard::Unavailable => return Inspection::Unavailable,
    };
    let legacy = probes.legacy(&guard);
    let root = probes.root(&guard);
    let owner = probes.owner(&guard);
    let inside = if root == Root::Absent {
        Observation::Absent
    } else {
        Observation::Unverified
    };
    Inspection::Held(ProfileFacts {
        legacy,
        root,
        owner,
        lock: inside,
        authority: inside,
        host: inside,
        history: inside,
        fresh_root: Destination::Unverified,
        fresh_namespace: Destination::Unverified,
    })
}

#[cfg(test)]
mod tests;
