//! Decisions for the reliability pass. Call sites use these so a test fails
//! when an error is treated as a missing value again.

use anyhow::Context;

/// `Err` stays `Err`. `Ok(None)` is not a stored height.
pub(crate) fn export_height_is_set<E>(read: Result<Option<u64>, E>) -> Result<bool, E> {
    Ok(read?.unwrap_or(0) > 0)
}

/// `Err` stays `Err`. `Ok(None)` is no stored export.
pub(crate) fn stored_export_height<E>(read: Result<Option<u64>, E>) -> Result<Option<u64>, E> {
    read
}

/// `Err` stays `Err`. A missing key is height 0.
pub(crate) fn stored_export_height_or_zero<E>(read: Result<Option<u64>, E>) -> Result<u64, E> {
    Ok(read?.unwrap_or(0))
}

/// `Err` stays `Err`. `Ok(None)` is a gap with no undo row.
pub(crate) fn connect_gap_read<T, E>(read: Result<Option<T>, E>) -> Result<Option<T>, E> {
    read
}

/// `Ok(true)` only when the file names the stored export height.
/// A failed export-height read is `Err` and does not delete the file.
pub(crate) fn startup_skips_refused_seed<E>(
    export_height: Result<Option<u64>, E>,
    refused_file: Option<u64>,
) -> Result<bool, E> {
    let export = export_height?;
    Ok(refused_file.is_some() && export == refused_file)
}

/// One peer at the failure cap stops. Two failures with one peer still retry.
pub(crate) fn single_peer_chunk_failure_stops(peers: usize, failures: u32, cap: u32) -> bool {
    peers <= 1 && failures >= cap
}

/// A fatal download does not reset the stall, including past the replay tip.
/// A stall with no fatal flag still resets past the replay tip.
pub(crate) fn stall_resets(download_fatal: bool, past_replay_tip: bool) -> bool {
    !download_fatal && past_replay_tip
}

/// A failed shutdown step is not a clean stop.
pub(crate) fn shutdown_step<T, E>(step: Result<T, E>) -> Result<T, E> {
    step
}

/// Clear the dirty flag only after the shutdown steps returned `Ok`.
pub(crate) fn clear_dirty_after_shutdown<E>(
    steps: Result<(), E>,
    clear: impl FnOnce(),
) -> Result<(), E> {
    steps?;
    clear();
    Ok(())
}

/// An export error shares the refused-checkpoint stop. Three stops. Fewer retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExportRetry {
    Sleep,
    Stop,
}

pub(crate) fn checkpoint_export_error(streak: u32) -> (u32, ExportRetry) {
    let streak = streak.saturating_add(1);
    if crate::storage::ibd_autorepair::checkpoint_refuse_hard_stop(streak) {
        (streak, ExportRetry::Stop)
    } else {
        (streak, ExportRetry::Sleep)
    }
}

/// The header timeout cap returns the error. It does not reset the counter.
pub(crate) fn header_timeout_stops(failures: u32, cap: u32) -> bool {
    failures >= cap
}

/// `Err` stays `Err`. A missing slot is the caller's `Ok(0)`.
pub(crate) fn resume_seed_slot<E>(read: Result<u8, E>) -> Result<u8, E> {
    read
}

/// `Err` stays `Err`. A short value stays `Ok(None)`.
pub(crate) fn resume_seed_count<E>(read: Result<Option<u64>, E>) -> Result<Option<u64>, E> {
    read
}

/// `Err` stays `Err`. A missing previous count is 0.
pub(crate) fn resume_seed_accepted<E>(read: Result<Option<u64>, E>) -> Result<u64, E> {
    Ok(read?.unwrap_or(0))
}

/// The progress task stops when the session flag drops or the tip is reached.
pub(crate) fn progress_task_should_stop(session_active: bool, reached_end: bool) -> bool {
    !session_active || reached_end
}

/// The checkpoint thread stops when the session asks or the export range is done.
pub(crate) fn checkpoint_thread_should_stop(stop_requested: bool, export_done: bool) -> bool {
    stop_requested || export_done
}

/// Write the engine dirty flag. A missing parent is `Err`.
pub(crate) fn mark_engine_dirty(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(path, b"1").with_context(|| format!("set engine dirty flag {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_export_height_does_not_select_the_chainstate_table() {
        assert!(export_height_is_set::<&str>(Err("read")).is_err());
        assert_eq!(export_height_is_set(Ok::<Option<u64>, &str>(None)).unwrap(), false);
        assert_eq!(export_height_is_set(Ok::<Option<u64>, &str>(Some(0))).unwrap(), false);
        assert_eq!(export_height_is_set(Ok::<Option<u64>, &str>(Some(8))).unwrap(), true);
    }

    #[test]
    fn a_failed_export_height_does_not_wipe_the_engine() {
        assert!(stored_export_height::<&str>(Err("read")).is_err());
        assert_eq!(stored_export_height(Ok::<Option<u64>, &str>(None)).unwrap(), None);
        assert!(stored_export_height_or_zero::<&str>(Err("read")).is_err());
        assert_eq!(stored_export_height_or_zero(Ok::<Option<u64>, &str>(None)).unwrap(), 0);
        assert_eq!(stored_export_height_or_zero(Ok::<Option<u64>, &str>(Some(9))).unwrap(), 9);
        let src = include_str!("parallel_ibd/mod.rs");
        assert_eq!(src.matches("stored_export_height_or_zero(").count(), 2);
        assert_eq!(src.matches("stored_export_height(").count(), 1);
        let mut rest = src;
        let mut sites = 0;
        while let Some(i) = rest.find("get_engine_export_height()") {
            sites += 1;
            let after = rest[i + "get_engine_export_height()".len()..].trim_start();
            assert!(
                after.starts_with(','),
                "export-height read {} is flattened: {}",
                sites,
                &after[..after.len().min(24)]
            );
            let tail = &rest[i..i + 500];
            assert!(
                tail.contains(")?;") || tail.contains("join_download_handles_after_grace"),
                "export-height read {sites} does not propagate"
            );
            rest = &rest[i + "get_engine_export_height()".len()..];
        }
        assert_eq!(sites, 3);
    }

    #[test]
    fn a_failed_undo_read_keeps_the_snapshot_journal() {
        assert!(connect_gap_read::<u64, &str>(Err("undo")).is_err());
        assert_eq!(connect_gap_read(Ok::<Option<u64>, &str>(None)).unwrap(), None);
        assert_eq!(connect_gap_read(Ok::<Option<u64>, &str>(Some(3))).unwrap(), Some(3));
        let src = include_str!("reorg_executor.rs");
        assert!(src.contains("connect_gap_read(\n                storage.blocks().get_hash_by_height(height),"));
        assert!(src.contains("connect_gap_read(\n                storage.blocks().get_undo_log(&hash),"));
    }

    #[test]
    fn a_refused_checkpoint_is_not_imported_at_startup() {
        assert!(startup_skips_refused_seed::<&str>(Err("height"), Some(10)).is_err());
        assert_eq!(
            startup_skips_refused_seed(Ok::<Option<u64>, &str>(Some(10)), Some(10)).unwrap(),
            true
        );
        assert_eq!(
            startup_skips_refused_seed(Ok::<Option<u64>, &str>(Some(9)), Some(10)).unwrap(),
            false
        );
        assert_eq!(startup_skips_refused_seed(Ok::<Option<u64>, &str>(None), None).unwrap(), false);
    }

    #[test]
    fn a_single_peer_chunk_failure_stops() {
        assert!(single_peer_chunk_failure_stops(1, 3, 3));
        assert!(!single_peer_chunk_failure_stops(1, 2, 3));
        assert!(!single_peer_chunk_failure_stops(2, 3, 3));
    }

    #[test]
    fn a_dead_worker_does_not_reset_the_stall() {
        assert!(!stall_resets(true, true));
        assert!(!stall_resets(true, false));
        assert!(stall_resets(false, true));
        assert!(!stall_resets(false, false));
    }

    #[test]
    fn a_failed_shutdown_flush_is_not_a_clean_stop() {
        assert!(shutdown_step::<(), &str>(Err("flush")).is_err());
        assert!(shutdown_step(Ok::<(), &str>(())).is_ok());
        let mut cleared = false;
        assert!(clear_dirty_after_shutdown(Err("flush"), || cleared = true).is_err());
        assert!(!cleared, "a flush error cleared the dirty flag");
        assert!(clear_dirty_after_shutdown(Ok::<(), &str>(()), || cleared = true).is_ok());
        assert!(cleared);
        let src = include_str!("parallel_ibd/validation_loop_parts/scoreboard.rs");
        assert_eq!(src.matches("clear_dirty_after_shutdown(").count(), 2);
        assert_eq!(src.matches("clear_engine_dirty_flag(").count(), 2);
    }

    #[test]
    fn a_header_timeout_stops_at_the_cap() {
        assert!(header_timeout_stops(3, 3));
        assert!(!header_timeout_stops(2, 3));
        let src = include_str!("parallel_ibd/headers.rs");
        let at_cap = src.find("header_timeout_stops(").expect("cap check");
        let stop = src[at_cap..].find("return Err").expect("cap returns");
        assert!(src[at_cap..at_cap + stop].contains("Too many header failures") || src[at_cap + stop..].contains("Too many header failures"));
        assert!(
            !src[at_cap..at_cap + stop].contains("consecutive_failures = 0"),
            "the header cap still resets and continues"
        );
    }

    #[test]
    fn a_failed_resume_seed_read_does_not_default_the_count() {
        assert!(resume_seed_slot::<&str>(Err("slot")).is_err());
        assert_eq!(resume_seed_slot(Ok::<u8, &str>(0u8)).unwrap(), 0);
        assert!(resume_seed_count::<&str>(Err("count")).is_err());
        assert_eq!(resume_seed_count(Ok::<Option<u64>, &str>(None)).unwrap(), None);
        assert!(resume_seed_accepted::<&str>(Err("prev")).is_err());
        assert_eq!(resume_seed_accepted(Ok::<Option<u64>, &str>(None)).unwrap(), 0);
        assert_eq!(resume_seed_accepted(Ok::<Option<u64>, &str>(Some(6))).unwrap(), 6);
        let src = include_str!("parallel_ibd/mod.rs");
        assert!(src.contains(
            "resume_seed_slot(\n                    storage.chain().get_engine_ckpt_slot(),\n                )?;"
        ));
        assert!(src.contains(
            "resume_seed_count(\n                    storage.chain().get_engine_export_utxo_count(),\n                )?;"
        ));
        assert!(src.contains(
            "resume_seed_accepted(\n                    storage.chain().get_engine_prev_accepted_utxo_count(),\n                )?;"
        ));
    }

    #[test]
    fn a_progress_task_stops_when_the_session_ends() {
        assert!(progress_task_should_stop(false, false));
        assert!(progress_task_should_stop(true, true));
        assert!(!progress_task_should_stop(true, false));
        let src = include_str!("parallel_ibd/mod.rs");
        assert!(src.contains("progress_task_should_stop("));
        assert_eq!(
            src.matches("h.abort();").count(),
            3,
            "an early return drops the progress task"
        );
    }

    #[test]
    fn a_checkpoint_thread_stops_when_the_session_returns() {
        assert!(checkpoint_thread_should_stop(true, false));
        assert!(checkpoint_thread_should_stop(false, true));
        assert!(!checkpoint_thread_should_stop(false, false));
    }

    #[test]
    fn a_dirty_flag_write_failure_is_not_a_clean_engine() {
        let dir = std::env::temp_dir().join(format!(
            "blvm-dirty-flag-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("engine.dirty");
        assert!(mark_engine_dirty(&path).is_ok());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        let missing = dir.join("no-such-parent").join("engine.dirty");
        assert!(mark_engine_dirty(&missing).is_err());
    }

    #[test]
    fn a_failed_checkpoint_export_stops_at_three() {
        let (streak, action) = checkpoint_export_error(1);
        assert_eq!(streak, 2);
        assert_eq!(action, ExportRetry::Sleep);
        let (streak, action) = checkpoint_export_error(2);
        assert_eq!(streak, 3);
        assert_eq!(action, ExportRetry::Stop);
        let src = include_str!("parallel_ibd/mod.rs");
        assert!(src.contains("checkpoint_export_error("));
        assert!(src.contains("ExportRetry::Stop"));
    }

    fn assert_arm_joins_downloads(src: &str, arm: &str) {
        let at = src.find(arm).unwrap_or_else(|| panic!("missing {arm}"));
        let end = src[at..]
            .find("return Err")
            .unwrap_or_else(|| panic!("{arm} has no return"));
        let window = &src[at..at + end];
        assert!(
            window.contains("join_download_handles_after_grace(download_handles).await"),
            "{arm} drops the download workers"
        );
    }

    #[test]
    fn a_download_error_return_joins_workers_after_grace() {
        let src = include_str!("parallel_ibd/mod.rs");
        let closure = src
            .split_once("let mut ibd_pipeline_shutdown")
            .expect("shutdown closure")
            .1
            .split_once("match tokio::task::block_in_place")
            .expect("validation join")
            .0;
        let requested = closure
            .find("assigner.request_shutdown()")
            .expect("closure requests shutdown");
        let feeder = closure
            .find("feeder_handle.join()")
            .expect("closure joins the feeder");
        assert!(
            requested < feeder,
            "the shutdown closure joins the feeder without stopping download workers"
        );
        for arm in [
            "Ok(Err(e)) => {",
            "Err(panic) => {",
            "if validated_through < effective_end_final {",
            "if durable_tip < effective_end_final {",
        ] {
            assert_arm_joins_downloads(src, arm);
        }
        let mut from = 0;
        let mut poison_arms = 0;
        while let Some(at) = src[from..].find("IBD UTXO mutex poisoned") {
            let abs = from + at;
            let start = src[..abs].rfind("Err(msg) =>").expect("poison arm");
            let window = &src[start..abs];
            assert!(
                window.contains("join_download_handles_after_grace(download_handles).await"),
                "a poisoned UTXO mutex drops the download workers"
            );
            poison_arms += 1;
            from = abs + "IBD UTXO mutex poisoned".len();
        }
        assert_eq!(poison_arms, 2);
    }
}
