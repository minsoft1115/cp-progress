//! Shared helpers for the PTY-based integration tests.

use std::fs::File;
use std::io::{ErrorKind, Read};

/// Read one chunk from a PTY master, retrying on EINTR and treating EIO (the slave side fully
/// closed) or any other error as end-of-stream. Returns 0 at EOF. This keeps the integration
/// tests from mistaking a signal-interrupted read for the end of cprog's output.
pub fn read_retry(master: &mut File, buf: &mut [u8]) -> usize {
    loop {
        return match master.read(buf) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
    }
}
