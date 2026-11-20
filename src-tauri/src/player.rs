//! External player process tracking.
//!
//! Electron killed the previous player before spawning a new one (`player?.kill()`).
//! This module preserves that behavior and ensures child processes are terminated on exit.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

#[derive(Default)]
pub struct PlayerState(pub Mutex<Option<Child>>);

impl PlayerState {
    /// Kill any existing player, then spawn `exe` with the validated streaming URL.
    pub fn spawn(&self, exe: &Path, url: &str) -> Result<(), String> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(mut previous) = guard.take() {
            let _ = previous.kill();
            let _ = previous.wait();
        }

        let child = Command::new(exe)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;

        *guard = Some(child);
        Ok(())
    }

    /// Kill the active player if any (used by close/quit flows).
    pub fn kill(&self) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for PlayerState {
    fn drop(&mut self) {
        if let Ok(guard) = self.0.get_mut() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
