use super::super::content::SharedSpanContext;
use super::super::filter::graph_visibility_closure;
use super::*;
use atomic_core::change::{VerifiedChange, VerifiedLoadError};
use atomic_core::pristine::{
    CapabilityMutTxnT, MutTxnT, TreeTxnT, ViewTxnT, SPAN_RANGE_CAPABILITY,
};
use atomic_core::types::GraphNode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Far longer than a render takes; only a render that is stuck behind the
/// build it is holding up ever waits this long.
const SECOND_RENDER_DEADLINE: Duration = Duration::from_secs(60);

fn record_all(repo: &TestRepository, message: &str) {
    repo.record(
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .sync_vault(false)
            .enrich_kg(false),
    )
    .unwrap();
}

fn enable_spans(repo: &TestRepository) {
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
    txn.commit().unwrap();
}

fn write_and_add(temp: &tempfile::TempDir, repo: &TestRepository, path: &str, bytes: &[u8]) {
    std::fs::write(temp.path().join(path), bytes).unwrap();
    repo.add(path, TrackingOptions::default()).unwrap();
}

/// `legacy.txt` stays graph-origin; `a.txt` and `b.txt` are span-rooted.
fn record_mixed_fixture(temp: &tempfile::TempDir, repo: &TestRepository) {
    write_and_add(temp, repo, "legacy.txt", b"legacy\n");
    record_all(repo, "graph origin");
    enable_spans(repo);
    write_and_add(temp, repo, "a.txt", b"alpha\n");
    write_and_add(temp, repo, "b.txt", b"beta\n");
    record_all(repo, "span files");
    std::fs::write(temp.path().join("a.txt"), b"alpha edited\n").unwrap();
    record_all(repo, "edit a");
}

/// Lets a test run a second render while the first span context build is
/// still inside the change store.
#[derive(Default)]
struct BuildGate {
    state: Mutex<BuildGateState>,
    changed: Condvar,
}

#[derive(Default)]
struct BuildGateState {
    build_started: bool,
    second_render_finished: bool,
    build_gave_up: bool,
}

impl BuildGate {
    /// Called from inside the first build: stay there until the second
    /// render has finished, or give up when it never does.
    fn hold_build(&self) {
        let mut state = self.state.lock().unwrap();
        state.build_started = true;
        self.changed.notify_all();
        let (mut state, wait) = self
            .changed
            .wait_timeout_while(state, SECOND_RENDER_DEADLINE, |state| {
                !state.second_render_finished
            })
            .unwrap();
        if wait.timed_out() {
            state.build_gave_up = true;
        }
    }

    fn wait_for_build(&self) {
        let state = self.state.lock().unwrap();
        let (_state, wait) = self
            .changed
            .wait_timeout_while(state, SECOND_RENDER_DEADLINE, |state| !state.build_started)
            .unwrap();
        assert!(!wait.timed_out(), "the first render never built a context");
    }

    fn second_render_finished(&self) {
        self.state.lock().unwrap().second_render_finished = true;
        self.changed.notify_all();
    }

    fn build_gave_up(&self) -> bool {
        self.state.lock().unwrap().build_gave_up
    }
}

/// The repository's change store, counting the verified loads that only a
/// span context build performs and optionally holding the first one up.
struct ObservedStore<'a> {
    inner: &'a ChangeStore,
    verified_loads: AtomicUsize,
    hold_first_build: Option<&'a BuildGate>,
}

impl<'a> ObservedStore<'a> {
    fn new(inner: &'a ChangeStore) -> Self {
        Self {
            inner,
            verified_loads: AtomicUsize::new(0),
            hold_first_build: None,
        }
    }

    fn holding_first_build(inner: &'a ChangeStore, gate: &'a BuildGate) -> Self {
        Self {
            hold_first_build: Some(gate),
            ..Self::new(inner)
        }
    }
}

impl atomic_core::change::ChangeStore for ObservedStore<'_> {
    type Error = ChangeStoreError;

    fn get_change(&self, hash: &Hash) -> Result<Change, Self::Error> {
        self.inner.load_change(hash)
    }

    fn get_verified_change(
        &self,
        hash: &Hash,
    ) -> Result<VerifiedChange, VerifiedLoadError<Self::Error>> {
        if self.verified_loads.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(gate) = self.hold_first_build {
                gate.hold_build();
            }
        }
        atomic_core::change::ChangeStore::get_verified_change(self.inner, hash)
    }

    fn get_contents<F>(
        &self,
        hash_fn: F,
        span: GraphNode<NodeId>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error>
    where
        F: Fn(NodeId) -> Option<Hash>,
    {
        atomic_core::change::ChangeStore::get_contents(self.inner, hash_fn, span, buf)
    }

    fn get_contents_ext(
        &self,
        span: GraphNode<Option<Hash>>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error> {
        atomic_core::change::ChangeStore::get_contents_ext(self.inner, span, buf)
    }
}

/// Building a span context runs rayon jobs, so the worker building it can end
/// up waiting for another worker — one that has meanwhile picked up a file of
/// the same walk and needs the context too. That second render has to finish
/// while the first build is still in progress; if it waits for the build, the
/// two wait on each other and the walk never returns.
#[test]
fn render_does_not_wait_for_a_span_context_build_in_progress() {
    let (temp, repo) = create_temp_repo();
    record_mixed_fixture(&temp, &repo);
    let txn = repo.pristine().read_txn().unwrap();
    let view = txn.get_view(repo.current_view()).unwrap().unwrap();
    let visibility = graph_visibility_closure(&txn, &view).unwrap();
    let a = txn.get_inode("a.txt").unwrap().unwrap();
    let b = txn.get_inode("b.txt").unwrap().unwrap();

    let gate = BuildGate::default();
    let store = ObservedStore::holding_first_build(repo.change_store(), &gate);
    let spans = SharedSpanContext::new(&txn, &store, &visibility);
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| spans.render_if_present(a));
        gate.wait_for_build();
        let second = spans.render_if_present(b);
        gate.second_render_finished();
        (first.join().unwrap(), second)
    });

    assert!(
        !gate.build_gave_up(),
        "the second render waited for the context build that was waiting for it"
    );
    assert_eq!(
        first.unwrap().as_deref(),
        Some(b"alpha edited\n".as_slice())
    );
    assert_eq!(second.unwrap().as_deref(), Some(b"beta\n".as_slice()));
}

/// A parallel walk primes the context before it fans out. Priming builds it
/// once when the walk covers a span-rooted file, and not at all for a walk
/// over graph-origin files only.
#[test]
fn prime_builds_the_span_context_once_and_only_for_span_rooted_files() {
    let (temp, repo) = create_temp_repo();
    record_mixed_fixture(&temp, &repo);
    let txn = repo.pristine().read_txn().unwrap();
    let view = txn.get_view(repo.current_view()).unwrap().unwrap();
    let visibility = graph_visibility_closure(&txn, &view).unwrap();
    let legacy = txn.get_inode("legacy.txt").unwrap().unwrap();
    let a = txn.get_inode("a.txt").unwrap().unwrap();
    let b = txn.get_inode("b.txt").unwrap().unwrap();

    let store = ObservedStore::new(repo.change_store());
    let spans = SharedSpanContext::new(&txn, &store, &visibility);
    spans.prime([legacy]);
    assert_eq!(
        store.verified_loads.load(Ordering::SeqCst),
        0,
        "graph-origin files need no span context"
    );
    assert_eq!(spans.render_if_present(legacy).unwrap(), None);

    spans.prime([legacy, a]);
    let one_build = store.verified_loads.load(Ordering::SeqCst);
    assert!(one_build > 0, "a span-rooted file needs the context built");

    spans.prime([a, b]);
    assert_eq!(
        spans.render_if_present(a).unwrap().as_deref(),
        Some(b"alpha edited\n".as_slice())
    );
    assert_eq!(
        spans.render_if_present(b).unwrap().as_deref(),
        Some(b"beta\n".as_slice())
    );
    assert_eq!(
        store.verified_loads.load(Ordering::SeqCst),
        one_build,
        "priming again and rendering reuse the one context"
    );
}

/// The view walk renders on rayon workers; span-rooted and graph-origin files
/// alike must come back with their recorded bytes.
#[test]
fn view_entries_walk_renders_mixed_views_on_a_worker_pool() {
    let (temp, repo) = create_temp_repo();
    record_mixed_fixture(&temp, &repo);
    let view = repo.current_view().to_string();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .unwrap();
    for _ in 0..20 {
        let mut rendered = std::collections::BTreeMap::new();
        pool.install(|| {
            repo.materialize_view_entries::<()>(&view, |entry| {
                if entry.kind == ViewEntryKind::File {
                    rendered.insert(entry.path, entry.content);
                }
                Ok(())
            })
        })
        .unwrap()
        .unwrap();
        assert_eq!(rendered["legacy.txt"], b"legacy\n");
        assert_eq!(rendered["a.txt"], b"alpha edited\n");
        assert_eq!(rendered["b.txt"], b"beta\n");
    }
}
