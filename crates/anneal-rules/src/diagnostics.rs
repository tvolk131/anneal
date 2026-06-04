use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// One rule-internal diagnostic timing span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleTiming {
    pub label: &'static str,
    pub duration: Duration,
}

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static TIMINGS: RefCell<Vec<RuleTiming>> = const { RefCell::new(Vec::new()) };
}

/// Start collecting rule-internal timings on the current thread.
pub fn start_rule_timings() {
    TIMINGS.with(|timings| timings.borrow_mut().clear());
    ENABLED.with(|enabled| enabled.set(true));
}

/// Stop collecting and return all rule-internal timings from the current thread.
pub fn take_rule_timings() -> Vec<RuleTiming> {
    ENABLED.with(|enabled| enabled.set(false));
    TIMINGS.with(|timings| std::mem::take(&mut *timings.borrow_mut()))
}

pub(crate) fn time<T>(label: &'static str, f: impl FnOnce() -> T) -> T {
    if !ENABLED.with(Cell::get) {
        return f();
    }

    let start = Instant::now();
    let result = f();
    let duration = start.elapsed();
    TIMINGS.with(|timings| {
        timings.borrow_mut().push(RuleTiming { label, duration });
    });
    result
}
