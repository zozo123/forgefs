//! Debug-build-only synchronization seams for deterministic process races.

#[cfg(debug_assertions)]
use forge_types::Error;
use forge_types::Result;

/// Wait until `participants` debug processes have reached the same transition.
///
/// Release builds contain no environment lookup or synchronization path. Tests
/// use a fresh directory per race, so process-id marker files cannot be stale.
pub(crate) fn process_barrier(
    environment: &str,
    participants: usize,
    transition: &str,
) -> Result<()> {
    #[cfg(debug_assertions)]
    if let Some(raw) = std::env::var_os(environment) {
        use std::fs::{self, OpenOptions};
        use std::path::PathBuf;
        use std::time::{Duration, Instant};

        let directory = PathBuf::from(raw);
        fs::create_dir_all(&directory)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(std::process::id().to_string()))?;

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let ready = fs::read_dir(&directory)?
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_type()
                        .map(|kind| kind.is_file())
                        .unwrap_or(false)
                })
                .count();
            if ready >= participants {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::Busy(format!(
                    "timed out waiting for {transition} test barrier"
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(not(debug_assertions))]
    let _ = (environment, participants, transition);
    Ok(())
}
