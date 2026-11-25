use super::*;
use Observation::{Absent, Matching, Unverified};

const EMPTY: ProfileFacts = ProfileFacts {
    root: Root::Absent,
    legacy: Absent,
    owner: Absent,
    lock: Absent,
    authority: Absent,
    host: Absent,
    history: Absent,
    fresh_root: Destination::Unverified,
    fresh_namespace: Destination::Unverified,
};
const READY: ProfileFacts = ProfileFacts {
    root: Root::Ready,
    owner: Matching,
    lock: Matching,
    authority: Matching,
    host: Matching,
    history: Matching,
    ..EMPTY
};

#[test]
fn first_open_needs_disjoint_targets_and_definite_absence() {
    let fresh = ProfileFacts {
        fresh_root: Destination::VerifiedDisjoint,
        fresh_namespace: Destination::VerifiedDisjoint,
        ..EMPTY
    };
    assert_eq!(Inspection::Held(fresh).classify(), Decision::FirstUse);
    for facts in [
        EMPTY,
        ProfileFacts {
            fresh_root: Destination::Unverified,
            ..fresh
        },
        ProfileFacts {
            fresh_namespace: Destination::Unverified,
            ..fresh
        },
        ProfileFacts {
            legacy: Unverified,
            ..fresh
        },
        ProfileFacts {
            root: Root::Unverified,
            ..fresh
        },
        ProfileFacts {
            owner: Matching,
            ..fresh
        },
        ProfileFacts {
            lock: Matching,
            ..fresh
        },
        ProfileFacts {
            history: Unverified,
            ..fresh
        },
    ] {
        assert_eq!(Inspection::Held(facts).classify(), Decision::Blocked);
    }
    assert_eq!(Inspection::Busy.classify(), Decision::Busy);
    assert_eq!(Inspection::Unavailable.classify(), Decision::Blocked);
}

#[test]
fn intact_profiles_open_but_substituted_or_mixed_stores_do_not() {
    let legacy = ProfileFacts {
        legacy: Matching,
        ..EMPTY
    };
    for facts in [legacy, READY] {
        assert_eq!(Inspection::Held(facts).classify(), Decision::OpenExisting);
    }
    for facts in [
        ProfileFacts {
            history: Unverified,
            ..READY
        },
        ProfileFacts {
            authority: Unverified,
            ..READY
        },
        ProfileFacts {
            host: Absent,
            ..READY
        },
        ProfileFacts {
            owner: Absent,
            ..READY
        },
        ProfileFacts {
            legacy: Matching,
            ..READY
        },
        ProfileFacts {
            owner: Unverified,
            ..legacy
        },
    ] {
        assert_eq!(Inspection::Held(facts).classify(), Decision::Blocked);
    }
}

#[test]
fn resume_requires_the_exact_durable_prefix_and_never_reissues_an_uncertain_write() {
    let before = ProfileFacts {
        root: Root::Pending(Step::Authority, Intent::Unstarted),
        owner: Matching,
        lock: Matching,
        ..EMPTY
    };
    let issued = ProfileFacts {
        root: Root::Pending(Step::Authority, Intent::Issued),
        ..before
    };
    for facts in [
        ProfileFacts {
            root: Root::Pending(Step::Owner, Intent::Unstarted),
            fresh_namespace: Destination::Unverified,
            ..EMPTY
        },
        before,
        ProfileFacts {
            authority: Matching,
            ..issued
        },
        ProfileFacts {
            root: Root::Pending(Step::Publish, Intent::Unstarted),
            ..READY
        },
    ] {
        assert_eq!(Inspection::Held(facts).classify(), Decision::ResumeSetup);
    }
    for facts in [
        issued,
        ProfileFacts {
            authority: Unverified,
            ..issued
        },
        ProfileFacts {
            authority: Matching,
            ..before
        },
        ProfileFacts {
            owner: Absent,
            ..before
        },
        ProfileFacts {
            history: Matching,
            ..before
        },
        ProfileFacts {
            root: Root::Pending(Step::Publish, Intent::Issued),
            ..READY
        },
    ] {
        assert_eq!(Inspection::Held(facts).classify(), Decision::Blocked);
    }
}

struct Fake {
    guard: fn() -> Guard<()>,
    legacy: Observation,
    root: Root,
    owner: Observation,
    log: std::cell::RefCell<Vec<&'static str>>,
}

struct Held<'a>(&'a std::cell::RefCell<Vec<&'static str>>);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.borrow_mut().push("release");
    }
}

impl<'a> Probes for &'a Fake {
    type Guard = Held<'a>;

    fn guard(&self) -> Guard<Held<'a>> {
        self.log.borrow_mut().push("guard");
        match (self.guard)() {
            Guard::Held(()) => Guard::Held(Held(&self.log)),
            Guard::Busy => Guard::Busy,
            Guard::Unavailable => Guard::Unavailable,
        }
    }

    fn legacy(&self, _: &Held<'a>) -> Observation {
        self.log.borrow_mut().push("legacy");
        self.legacy
    }

    fn root(&self, _: &Held<'a>) -> Root {
        self.log.borrow_mut().push("root");
        self.root
    }

    fn owner(&self, _: &Held<'a>) -> Observation {
        self.log.borrow_mut().push("owner");
        self.owner
    }
}

fn fake(legacy: Observation, root: Root, owner: Observation) -> Fake {
    Fake {
        guard: || Guard::Held(()),
        legacy,
        root,
        owner,
        log: std::cell::RefCell::default(),
    }
}

#[test]
fn a_guard_not_taken_samples_nothing() {
    let busy: fn() -> Guard<()> = || Guard::Busy;
    for (guard, expected) in [
        (busy, Decision::Busy),
        (|| Guard::Unavailable, Decision::Blocked),
    ] {
        let probes = Fake {
            guard,
            ..fake(Matching, Root::Absent, Absent)
        };
        let inspection = observe(&&probes);
        assert!(!matches!(inspection, Inspection::Held(_)));
        assert_eq!(inspection.classify(), expected);
        assert_eq!(*probes.log.borrow(), ["guard"]);
    }
}

#[test]
fn every_fact_is_sampled_while_the_guard_is_held() {
    let probes = fake(Matching, Root::Absent, Absent);
    observe(&&probes);
    assert_eq!(
        *probes.log.borrow(),
        ["guard", "legacy", "root", "owner", "release"]
    );
}

#[test]
fn objects_inside_the_root_are_absent_only_with_the_root() {
    for (root, inside) in [
        (Root::Absent, Absent),
        (Root::Unverified, Unverified),
        (Root::Ready, Unverified),
        (Root::Pending(Step::Lock, Intent::Issued), Unverified),
    ] {
        let Inspection::Held(facts) = observe(&&fake(Absent, root, Absent)) else {
            panic!("the guard was held");
        };
        assert_eq!(facts.root, root);
        assert_eq!(
            [facts.lock, facts.authority, facts.host, facts.history],
            [inside; 4]
        );
        assert_eq!(facts.fresh_root, Destination::Unverified);
        assert_eq!(facts.fresh_namespace, Destination::Unverified);
    }
}

#[test]
fn only_an_intact_legacy_installation_with_nothing_new_reopens() {
    assert_eq!(
        observe(&&fake(Matching, Root::Absent, Absent)).classify(),
        Decision::OpenExisting
    );
    for (legacy, root, owner) in [
        (Absent, Root::Absent, Absent),
        (Unverified, Root::Absent, Absent),
        (Matching, Root::Unverified, Absent),
        (Matching, Root::Absent, Unverified),
        (Matching, Root::Absent, Matching),
        (Absent, Root::Ready, Matching),
    ] {
        assert_eq!(
            observe(&&fake(legacy, root, owner)).classify(),
            Decision::Blocked
        );
    }
}
