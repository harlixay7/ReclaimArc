//! Recovery-unit state machine.
//!
//! The master-plan lifecycle:
//! PENDING → EXTRACTING → OUTPUT_WRITTEN → OUTPUT_VERIFIED → OUTPUT_DURABLE
//! → COMMITTED → RECLAIM_INTENT → RECLAIMED
//!
//! Every transition is durable (journaled before the corresponding
//! filesystem action). Transitions are strictly linear — no skipping, no
//! going backwards.

use reclaimarc_journal::UnitState;

use crate::error::CoreError;

/// All legal transitions.
pub const TRANSITIONS: &[(UnitState, UnitState)] = &[
    (UnitState::Pending, UnitState::Extracting),
    (UnitState::Extracting, UnitState::OutputWritten),
    (UnitState::OutputWritten, UnitState::OutputVerified),
    (UnitState::OutputVerified, UnitState::OutputDurable),
    (UnitState::OutputDurable, UnitState::Committed),
    (UnitState::Committed, UnitState::ReclaimIntent),
    (UnitState::ReclaimIntent, UnitState::Reclaimed),
];

/// The state after `state` in the canonical lifecycle.
pub fn next(state: UnitState) -> Result<UnitState, CoreError> {
    TRANSITIONS
        .iter()
        .find(|(from, _)| *from == state)
        .map(|(_, to)| *to)
        .ok_or_else(|| CoreError::Precondition(format!("{:?} is a terminal state", state)))
}

/// Whether a transition from `from` to `to` is legal.
pub fn can_transition(from: UnitState, to: UnitState) -> bool {
    TRANSITIONS.iter().any(|(f, t)| *f == from && *t == to)
}

/// Whether the unit has produced durable output.
pub fn is_committed(state: UnitState) -> bool {
    matches!(
        state,
        UnitState::Committed | UnitState::ReclaimIntent | UnitState::Reclaimed
    )
}

/// Whether the unit's source may be reclaimed.
pub fn is_reclaimed(state: UnitState) -> bool {
    state == UnitState::Reclaimed
}

/// The state a unit should be set to when extraction of it fails: it stays
/// EXTRACTING (resumable) unless the failure was structural.
pub fn failure_state() -> UnitState {
    UnitState::Extracting
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn full_lifecycle_is_linear() {
        let mut state = UnitState::Pending;
        for (from, to) in TRANSITIONS {
            assert_eq!(*from, state);
            assert!(can_transition(state, *to));
            state = *to;
        }
        assert_eq!(state, UnitState::Reclaimed);
        assert!(is_reclaimed(state));
    }

    #[test]
    fn illegal_transitions_rejected() {
        assert!(!can_transition(UnitState::Pending, UnitState::Committed));
        assert!(!can_transition(UnitState::Reclaimed, UnitState::Pending));
        assert!(!can_transition(UnitState::Extracting, UnitState::Pending));
        assert!(next(UnitState::Reclaimed).is_err());
    }

    proptest! {
        #[test]
        fn next_is_always_legal(from in proptest::sample::select(UnitState::ALL.to_vec())) {
            if let Ok(to) = next(from) {
                prop_assert!(can_transition(from, to));
            }
        }
    }
}
