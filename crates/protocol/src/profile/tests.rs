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
