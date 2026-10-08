use generic_bootctl_core::*;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
struct Memory {
    state: State,
    merge: Merge,
    writes: Arc<AtomicUsize>,
}
impl Backend for Memory {
    fn slot_count(&self) -> u32 {
        self.state.slots.len() as u32
    }
    fn read_state(&mut self) -> io::Result<State> {
        Ok(self.state.clone())
    }
    fn commit(&mut self, state: &State, _: Operation) -> io::Result<()> {
        self.state = state.clone();
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn read_merge(&mut self) -> io::Result<Merge> {
        Ok(self.merge)
    }
    fn write_merge(&mut self, m: Merge) -> io::Result<()> {
        self.merge = m;
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
fn fixture() -> (Service, Arc<AtomicUsize>, Arc<AtomicBool>) {
    let writes = Arc::new(AtomicUsize::new(0));
    let memory = Memory {
        state: State {
            slots: vec![
                Slot {
                    priority: 15,
                    tries: 1,
                    successful: true,
                    verity_corrupted: true
                };
                2
            ],
            max_priority: 15,
            max_tries: 6,
            clear_successful_on_activate: false,
            successful_requires_bootable: true,
            health_on_success: HealthOnSuccess::ResetToOne,
        },
        merge: Merge {
            status: MergeStatus::Snapshotted,
            source: 1,
        },
        writes: writes.clone(),
    };
    let gate = Arc::new(AtomicBool::new(false));
    let g = gate.clone();
    (
        Service::new(
            Box::new(memory),
            0,
            Box::new(move || g.load(Ordering::Relaxed)),
        ),
        writes,
        gate,
    )
}
#[test]
fn write_gate_never_calls_backend_mutation() {
    let (mut s, writes, g) = fixture();
    assert_eq!(s.mark_successful(), Err(COMMAND_FAILED));
    assert_eq!(s.set_active(1), Err(COMMAND_FAILED));
    assert_eq!(s.set_unbootable(0), Err(COMMAND_FAILED));
    assert_eq!(
        s.set_merge_status(MergeStatus::Merging),
        Err(COMMAND_FAILED)
    );
    assert_eq!(writes.load(Ordering::Relaxed), 0);
    g.store(true, Ordering::Relaxed);
    s.mark_successful().unwrap();
    g.store(false, Ordering::Relaxed);
    assert_eq!(s.mark_successful(), Err(COMMAND_FAILED));
    assert_eq!(writes.load(Ordering::Relaxed), 1);
}
#[test]
fn misc_semantics_and_invalid_slots() {
    let (mut s, _, g) = fixture();
    g.store(true, Ordering::Relaxed);
    assert_eq!(s.active_slot(), Ok(0));
    assert_eq!(s.number_slots(), Ok(2));
    assert_eq!(s.suffix(99), Ok(c""));
    assert_eq!(s.suffix(1), Ok(c"_b"));
    for slot in [2, u32::MAX] {
        assert_eq!(s.is_bootable(slot), Err(INVALID_SLOT));
        assert_eq!(s.is_successful(slot), Err(INVALID_SLOT));
        assert_eq!(s.set_active(slot), Err(INVALID_SLOT));
        assert_eq!(s.set_unbootable(slot), Err(INVALID_SLOT));
    }
    s.set_active(1).unwrap();
    assert_eq!(s.active_slot(), Ok(1));
    assert_eq!(s.is_successful(1), Ok(true));
    s.set_unbootable(0).unwrap();
    assert_eq!(s.is_bootable(0), Ok(false));
    assert_eq!(s.is_successful(0), Ok(false));
    s.mark_successful().unwrap();
    assert_eq!(s.is_bootable(0), Ok(true));
    assert_eq!(s.is_successful(0), Ok(true));
}
#[test]
fn merge_status_and_frozen_transactions() {
    let (mut s, _, g) = fixture();
    g.store(true, Ordering::Relaxed);
    assert_eq!(s.merge_status(), Ok(MergeStatus::Snapshotted));
    for status in [
        MergeStatus::None,
        MergeStatus::Unknown,
        MergeStatus::Snapshotted,
        MergeStatus::Merging,
        MergeStatus::Cancelled,
    ] {
        s.set_merge_status(status).unwrap();
        assert_eq!(
            s.merge_status(),
            Ok(if status == MergeStatus::Snapshotted {
                MergeStatus::None
            } else {
                status
            })
        );
    }
    assert_eq!(s.execute(11, 5), Err(COMMAND_FAILED));
    assert_eq!(s.execute(5, -1), Ok(Reply::Text(c"")));
    assert_eq!(s.execute(6, -1), Err(INVALID_SLOT));
    assert_eq!(s.execute(16_777_215, 0), Ok(Reply::Int(1)));
}

struct Prepared {
    state: State,
    count: u32,
    fail_prepare: bool,
    calls: Arc<AtomicUsize>,
    health: Arc<AtomicUsize>,
}

impl Backend for Prepared {
    fn slot_count(&self) -> u32 {
        self.count
    }
    fn prepare(&mut self) -> io::Result<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail_prepare {
            Err(io::Error::other("storage unavailable"))
        } else {
            Ok(())
        }
    }
    fn read_state(&mut self) -> io::Result<State> {
        assert_eq!(self.calls.load(Ordering::Relaxed), 1);
        self.calls.fetch_add(10, Ordering::Relaxed);
        Ok(self.state.clone())
    }
    fn commit(&mut self, state: &State, _: Operation) -> io::Result<()> {
        assert_eq!(self.calls.load(Ordering::Relaxed), 11);
        self.calls.fetch_add(100, Ordering::Relaxed);
        let slot = state.slots[0];
        self.health.store(
            (usize::from(slot.priority) << 16)
                | (usize::from(slot.tries) << 8)
                | usize::from(slot.successful),
            Ordering::Relaxed,
        );
        assert_eq!(state.slots[1], self.state.slots[1]);
        Ok(())
    }
    fn read_merge(&mut self) -> io::Result<Merge> {
        assert_eq!(self.calls.load(Ordering::Relaxed), 1);
        self.calls.fetch_add(1000, Ordering::Relaxed);
        Ok(Merge {
            status: MergeStatus::None,
            source: 0,
        })
    }
    fn write_merge(&mut self, _: Merge) -> io::Result<()> {
        assert_eq!(self.calls.load(Ordering::Relaxed), 1);
        self.calls.fetch_add(10000, Ordering::Relaxed);
        Ok(())
    }
}

fn prepared() -> Prepared {
    Prepared {
        state: State {
            slots: vec![
                Slot {
                    priority: 15,
                    tries: 6,
                    successful: false,
                    verity_corrupted: false
                };
                2
            ],
            max_priority: 15,
            max_tries: 6,
            clear_successful_on_activate: false,
            successful_requires_bootable: true,
            health_on_success: HealthOnSuccess::default(),
        },
        count: 2,
        fail_prepare: false,
        calls: Arc::new(AtomicUsize::new(0)),
        health: Arc::new(AtomicUsize::new(0)),
    }
}

#[test]
fn cached_metadata_and_invalid_slots_do_not_prepare_or_read() {
    let mut backend = prepared();
    backend.fail_prepare = true;
    let calls = backend.calls.clone();
    let mut service = Service::new(Box::new(backend), 0, Box::new(|| true));
    assert_eq!(service.execute(2, 0), Ok(Reply::Int(0)));
    assert_eq!(service.execute(3, 0), Ok(Reply::Int(2)));
    assert_eq!(service.execute(5, 1), Ok(Reply::Text(c"_b")));
    assert_eq!(service.execute(5, 2), Ok(Reply::Text(c"")));
    assert_eq!(service.execute(16_777_215, 0), Ok(Reply::Int(1)));
    assert_eq!(
        service.execute(16_777_214, 0),
        Ok(Reply::Text(c"2400346954240a5de495a1debc81429dd012d7b7"))
    );
    for slot in [2, u32::MAX] {
        assert_eq!(service.is_bootable(slot), Err(INVALID_SLOT));
        assert_eq!(service.is_successful(slot), Err(INVALID_SLOT));
        assert_eq!(service.set_active(slot), Err(INVALID_SLOT));
        assert_eq!(service.set_unbootable(slot), Err(INVALID_SLOT));
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn preparation_precedes_every_state_or_merge_access() {
    for (code, expected) in [
        (1, 11),
        (4, 1001),
        (6, 11),
        (7, 11),
        (8, 111),
        (9, 111),
        (10, 111),
        (11, 10001),
    ] {
        for fail_prepare in [false, true] {
            let mut backend = prepared();
            backend.fail_prepare = fail_prepare;
            // Activating slot zero demotes slot one, unlike success/unbootable.
            if code == 9 {
                backend.state.slots[1].priority = 14;
            }
            let calls = backend.calls.clone();
            let mut service = Service::new(Box::new(backend), 0, Box::new(|| true));
            let result = service.execute(code, 0);
            if fail_prepare {
                assert_eq!(result, Err(COMMAND_FAILED), "code {code}");
                assert_eq!(calls.load(Ordering::Relaxed), 1, "code {code}");
            } else {
                assert!(result.is_ok(), "code {code}: {result:?}");
                assert_eq!(calls.load(Ordering::Relaxed), expected, "code {code}");
            }
        }
    }
}

#[test]
fn mark_successful_preserves_or_resets_native_health_as_selected() {
    for policy in [
        HealthOnSuccess::ResetToOne,
        HealthOnSuccess::PreserveNonZero,
    ] {
        for priority in [0, 1, 15] {
            for tries in [0, 1, 6] {
                let mut backend = prepared();
                backend.state.health_on_success = policy;
                backend.state.slots[0].priority = priority;
                backend.state.slots[0].tries = tries;
                let health = backend.health.clone();
                let mut service = Service::new(Box::new(backend), 0, Box::new(|| true));
                service.mark_successful().unwrap();
                let (priority, tries) = match policy {
                    HealthOnSuccess::ResetToOne => (priority, 1),
                    HealthOnSuccess::PreserveNonZero => (priority.max(1), tries.max(1)),
                };
                assert_eq!(
                    health.load(Ordering::Relaxed),
                    (usize::from(priority) << 16) | (usize::from(tries) << 8) | 1
                );
            }
        }
    }
}

#[test]
fn changed_state_slot_count_fails_without_committing() {
    let mut backend = prepared();
    backend.state.slots.pop();
    let calls = backend.calls.clone();
    let mut service = Service::new(Box::new(backend), 0, Box::new(|| true));
    assert_eq!(service.mark_successful(), Err(COMMAND_FAILED));
    assert_eq!(calls.load(Ordering::Relaxed), 11);
}
