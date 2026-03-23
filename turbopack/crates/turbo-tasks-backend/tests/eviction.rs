#![feature(arbitrary_self_types)]
#![feature(arbitrary_self_types_pointers)]
#![allow(clippy::needless_return)] // tokio macro-generated code doesn't respect this

use std::sync::Arc;

use anyhow::Result;
use turbo_tasks::{
    ResolvedVc, State, TurboTasks, Vc, unmark_top_level_task_may_leak_eventually_consistent_state,
};
use turbo_tasks_backend::{BackendOptions, GitVersionInfo, TurboBackingStorage, TurboTasksBackend};

fn create_tt(name: &str) -> Arc<TurboTasks<TurboTasksBackend<TurboBackingStorage>>> {
    use std::hash::BuildHasher;
    let path = std::path::PathBuf::from(format!(
        "{}/.cache/{}",
        env!("CARGO_TARGET_TMPDIR"),
        rustc_hash::FxBuildHasher.hash_one(name)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    TurboTasks::new(TurboTasksBackend::new(
        BackendOptions {
            num_workers: Some(2),
            small_preallocation: true,
            storage_mode: Some(turbo_tasks_backend::StorageMode::ReadWrite),
            evict_after_snapshot: true,
            ..Default::default()
        },
        turbo_tasks_backend::turbo_backing_storage(
            path.as_path(),
            &GitVersionInfo {
                describe: "test-unversioned",
                dirty: false,
            },
            false,
            true,
        )
        .unwrap()
        .0,
    ))
}

/// Verify that after eviction, task re-execution produces correct results.
/// This tests the snapshot → evict → invalidate → restore → re-execute cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_recompute() {
    let tt = create_tt("eviction_recompute");
    let tt2 = tt.clone();

    let result = turbo_tasks::run_once(tt.clone(), async move {
        unmark_top_level_task_may_leak_eventually_consistent_state();

        // Create state via operation (persistent task)
        let state_op = create_state(1);
        let state_vc = state_op.resolve_strongly_consistent().await?;
        let state = state_op.read_strongly_consistent().await?;

        // Create compute task (persistent, depends on state)
        let output = compute(state_vc);
        let read = output.read_strongly_consistent().await?;
        assert_eq!(read.value, 1);
        let initial_random = read.random;

        // Trigger snapshot + eviction
        let (had_data, full, data_only) = tt2.backend().snapshot_and_evict(&*tt2);
        println!("snapshot had_data={had_data}, evicted: full={full}, data_only={data_only}");
        assert!(had_data, "snapshot should have persisted data");

        // Invalidate via state change — this requires restoring evicted tasks
        state.set(2);

        // Read again — tasks must be restored from disk before re-executing
        let read = output.read_strongly_consistent().await?;
        assert_eq!(read.value, 2);
        assert_ne!(read.random, initial_random);

        anyhow::Ok(())
    })
    .await;
    tt.stop_and_wait().await;
    result.unwrap();
}

/// Verify that eviction works with a deep (4-level) dependency chain.
/// Multiple intermediate tasks should be evicted and restored correctly.
/// Chain: create_state → add_one → times_three → plus_ten → deep_chain
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_deep_chain() {
    let tt = create_tt("eviction_deep_chain");
    let tt2 = tt.clone();

    let result = turbo_tasks::run_once(tt.clone(), async move {
        unmark_top_level_task_may_leak_eventually_consistent_state();

        let state_op = create_state(10);
        let state_vc = state_op.resolve_strongly_consistent().await?;
        let state = state_op.read_strongly_consistent().await?;

        let output = deep_chain(state_vc);
        let read = output.read_strongly_consistent().await?;
        // (10+1)*3+10 = 43
        assert_eq!(read.value, 43);
        let initial_random = read.random;

        // Snapshot + evict — expect multiple intermediate tasks evicted
        let (had_data, full, data_only) = tt2.backend().snapshot_and_evict(&*tt2);
        println!(
            "deep_chain: snapshot had_data={had_data}, evicted: full={full}, data_only={data_only}"
        );
        assert!(had_data, "snapshot should have persisted data");
        assert!(full + data_only > 0, "expected some tasks to be evicted");

        // Change the deepest input — must propagate through all restored tasks
        state.set(20);

        let read = output.read_strongly_consistent().await?;
        // (20+1)*3+10 = 73
        assert_eq!(read.value, 73);
        assert_ne!(read.random, initial_random);
        let random_after_first = read.random;

        // Evict again and change again
        let (had_data2, full2, data_only2) = tt2.backend().snapshot_and_evict(&*tt2);
        println!(
            "deep_chain (2nd): snapshot had_data={had_data2}, evicted: full={full2}, \
             data_only={data_only2}"
        );

        state.set(0);

        let read = output.read_strongly_consistent().await?;
        // (0+1)*3+10 = 13
        assert_eq!(read.value, 13);
        assert_ne!(read.random, random_after_first);

        anyhow::Ok(())
    })
    .await;
    tt.stop_and_wait().await;
    result.unwrap();
}

/// Verify that eviction + restore preserves dependency edges correctly.
/// After eviction, changing a deep dependency should still propagate
/// through the entire chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_dependency_chain() {
    let tt = create_tt("eviction_dependency_chain");
    let tt2 = tt.clone();

    let result = turbo_tasks::run_once(tt.clone(), async move {
        unmark_top_level_task_may_leak_eventually_consistent_state();

        let state_op = create_state(10);
        let state_vc = state_op.resolve_strongly_consistent().await?;
        let state = state_op.read_strongly_consistent().await?;

        let output = compute_chain(state_vc);
        let read = output.read_strongly_consistent().await?;
        assert_eq!(read.value, 20); // 10 * 2
        let initial_random = read.random;

        // Snapshot + evict
        let (had_data, full, data_only) = tt2.backend().snapshot_and_evict(&*tt2);
        println!("snapshot had_data={had_data}, evicted: full={full}, data_only={data_only}");
        assert!(had_data, "snapshot should have persisted data");

        assert!(full + data_only > 0, "expected some tasks to be evicted");

        // Change the deepest input
        state.set(5);

        let read = output.read_strongly_consistent().await?;
        assert_eq!(read.value, 10); // 5 * 2
        assert_ne!(read.random, initial_random);
        let random_after_first = read.random;

        // Evict again
        let (had_data2, full2, data_only2) = tt2.backend().snapshot_and_evict(&*tt2);
        println!(
            "snapshot (2nd) had_data={had_data2}, evicted: full={full2}, data_only={data_only2}"
        );

        // Change again
        state.set(100);

        let read = output.read_strongly_consistent().await?;
        assert_eq!(read.value, 200); // 100 * 2
        assert_ne!(read.random, random_after_first);

        anyhow::Ok(())
    })
    .await;
    tt.stop_and_wait().await;
    result.unwrap();
}

#[turbo_tasks::value(transparent)]
struct Step(State<u32>);

#[turbo_tasks::function(operation)]
fn create_state(initial: u32) -> Vc<Step> {
    Step(State::new(initial)).cell()
}

#[turbo_tasks::value]
struct Output {
    value: u32,
    random: u32,
}

#[turbo_tasks::function(operation)]
async fn compute(input: ResolvedVc<Step>) -> Result<Vc<Output>> {
    let value = *input.await?.get();
    Ok(Output {
        value,
        random: rand::random(),
    }
    .cell())
}

/// Inner function in the dependency chain
#[turbo_tasks::function(operation)]
async fn double(input: ResolvedVc<Step>) -> Result<Vc<u32>> {
    let value = *input.await?.get();
    Ok(Vc::cell(value * 2))
}

/// Outer function that depends on `double`
#[turbo_tasks::function(operation)]
async fn compute_chain(input: ResolvedVc<Step>) -> Result<Vc<Output>> {
    let doubled = double(input);
    let value = *doubled.connect().await?;
    Ok(Output {
        value,
        random: rand::random(),
    }
    .cell())
}

// =========================================================================
// Deep chain helpers — each layer reads the previous layer's output
// =========================================================================

#[turbo_tasks::function(operation)]
async fn add_one(input: ResolvedVc<Step>) -> Result<Vc<u32>> {
    let value = *input.await?.get();
    Ok(Vc::cell(value + 1))
}

#[turbo_tasks::function(operation)]
async fn times_three(input: ResolvedVc<u32>) -> Result<Vc<u32>> {
    let value = *input.await?;
    Ok(Vc::cell(value * 3))
}

#[turbo_tasks::function(operation)]
async fn plus_ten(input: ResolvedVc<u32>) -> Result<Vc<u32>> {
    let value = *input.await?;
    Ok(Vc::cell(value + 10))
}

#[turbo_tasks::function(operation)]
async fn deep_chain(input: ResolvedVc<Step>) -> Result<Vc<Output>> {
    // input → add_one → times_three → plus_ten → Output
    // For input=10: (10+1)*3+10 = 43
    let a = add_one(input).resolve_strongly_consistent().await?;
    let b = times_three(a).resolve_strongly_consistent().await?;
    let c = plus_ten(b).resolve_strongly_consistent().await?;
    let value = *c.await?;
    Ok(Output {
        value,
        random: rand::random(),
    }
    .cell())
}
