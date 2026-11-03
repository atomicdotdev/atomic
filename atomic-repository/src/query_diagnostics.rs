//! Opt-in, thread-local query resource measurements for tests and benchmarks.
//!
//! These counters describe consumer buffers, not ChangeStore caches, renderer
//! span buffers, graph metadata, or semantic-merge scratch. Measure synchronous
//! repository calls on the calling thread; do not nest measurement scopes.

use std::cell::RefCell;

/// Consumer resource high-water marks for a synchronous recorded query.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct QueryResources {
    pub files_retrieved: usize,
    pub bytes_retained: usize,
    pub output_buffer_high_water: usize,
    pub displayed_records_high_water: usize,
}

thread_local! {
    static RESOURCES: RefCell<Option<QueryResources>> = const { RefCell::new(None) };
}

/// Run one synchronous query and collect its consumer resource counters.
///
/// # Examples
///
/// ```
/// use atomic_repository::query_diagnostics::measure;
/// let (answer, resources) = measure(|| 42);
/// assert_eq!(answer, 42);
/// assert_eq!(resources.files_retrieved, 0);
/// ```
///
/// # Panics
/// Panics if measurements are nested on the same thread.
pub fn measure<T>(query: impl FnOnce() -> T) -> (T, QueryResources) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RESOURCES.with(|slot| *slot.borrow_mut() = None);
        }
    }
    RESOURCES.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(slot.is_none(), "query measurements cannot be nested");
        *slot = Some(QueryResources::default());
    });
    let _reset = Reset;
    let result = query();
    let stats = RESOURCES.with(|slot| slot.borrow_mut().take().expect("measurement is active"));
    (result, stats)
}

pub(crate) fn retrieved(bytes: usize) {
    RESOURCES.with(|slot| {
        if let Some(stats) = slot.borrow_mut().as_mut() {
            stats.files_retrieved += 1;
            stats.bytes_retained += bytes;
            stats.output_buffer_high_water = stats.output_buffer_high_water.max(bytes);
        }
    });
}

pub(crate) fn retained(count: usize) {
    RESOURCES.with(|slot| {
        if let Some(stats) = slot.borrow_mut().as_mut() {
            stats.displayed_records_high_water = stats.displayed_records_high_water.max(count);
        }
    });
}
