//! Native event gate; the caller holds MagiskD's serialized boot-stage lock.
//! Runtime completion alone is insufficient: native callbacks and projection
//! must also succeed before the outer event is completed and acknowledged.
use egysk_runtime::Stage;

#[derive(Default)]
pub struct BootState {
    completed: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    Duplicate,
    Execute(Stage),
}

impl BootState {
    pub fn admit(&self, stage: Stage) -> Result<Admission, &'static str> {
        let index = Stage::ALL
            .iter()
            .position(|candidate| *candidate == stage)
            .expect("runtime stage missing from Stage::ALL");
        if index < self.completed {
            Ok(Admission::Duplicate)
        } else if Stage::ALL.get(self.completed) == Some(&stage) {
            Ok(Admission::Execute(stage))
        } else {
            Err("out-of-order stage")
        }
    }

    pub fn complete(&mut self, stage: Stage) {
        assert_eq!(
            Stage::ALL.get(self.completed),
            Some(&stage),
            "completion without admitted stage"
        );
        self.completed += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_duplicates_and_real_event_barrier() {
        let mut state = BootState::default();
        assert_eq!(state.admit(Stage::PostFsData), Err("out-of-order stage"));
        for stage in Stage::ALL {
            assert_eq!(state.admit(stage), Ok(Admission::Execute(stage)));
            state.complete(stage);
            assert_eq!(state.admit(stage), Ok(Admission::Duplicate));
        }
    }

    #[test]
    fn failed_work_does_not_complete_outer_event() {
        let state = BootState::default();
        assert_eq!(
            state.admit(Stage::EarlyInit),
            Ok(Admission::Execute(Stage::EarlyInit))
        );
        // Failed work never calls complete, even if runtime preparation succeeded.
        assert_eq!(
            state.admit(Stage::EarlyInit),
            Ok(Admission::Execute(Stage::EarlyInit))
        );
        assert_eq!(state.admit(Stage::Init), Err("out-of-order stage"));
    }
}
