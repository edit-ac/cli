//! Explicit local wakeups over a shared committed journal. No semantic bytes or receipts.
use crate::ports::PortError;

pub trait LocalChannel {
    fn notify(&mut self, writer: u32) -> Result<(), PortError>;
    fn receive(&mut self) -> Result<Vec<u32>, PortError>;
}

/// Prepared bounded adapter; native process transport is separately qualified.
pub struct MemoryChannel {
    incoming: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    outgoing: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    capacity: usize,
}
impl MemoryChannel {
    pub fn pair(capacity: usize) -> Result<(Self, Self), PortError> {
        if capacity == 0 || capacity > 64 {
            return Err(PortError::ResourceLimit);
        }
        let a = Default::default();
        let b = Default::default();
        Ok((
            Self {
                incoming: std::sync::Arc::clone(&a),
                outgoing: std::sync::Arc::clone(&b),
                capacity,
            },
            Self {
                incoming: b,
                outgoing: a,
                capacity,
            },
        ))
    }
}
impl LocalChannel for MemoryChannel {
    fn notify(&mut self, writer: u32) -> Result<(), PortError> {
        if writer == 0 {
            return Err(PortError::InvalidRecord);
        }
        let mut queue = self.outgoing.lock().map_err(|_| PortError::Unavailable)?;
        if queue.len() >= self.capacity {
            return Err(PortError::Backpressure);
        }
        queue.push(writer);
        Ok(())
    }
    fn receive(&mut self) -> Result<Vec<u32>, PortError> {
        Ok(std::mem::take(
            &mut *self.incoming.lock().map_err(|_| PortError::Unavailable)?,
        ))
    }
}

#[cfg(unix)]
pub mod unix {
    use super::*;
    use editac_core::document::DocumentIdentity;
    use sha2::{Digest, Sha256};
    use std::{
        os::unix::{fs::MetadataExt, net::UnixDatagram},
        path::{Path, PathBuf},
    };

    pub struct UnixLocalChannel {
        socket: UnixDatagram,
        path: PathBuf,
        inode: u64,
        scope: [u8; 32],
        peers: Vec<PathBuf>,
    }
    fn io(error: std::io::Error) -> PortError {
        match error.kind() {
            std::io::ErrorKind::WouldBlock => PortError::Backpressure,
            std::io::ErrorKind::PermissionDenied => PortError::Permission,
            _ => PortError::Unavailable,
        }
    }
    impl UnixLocalChannel {
        /// Directory/path permission and exclusive ownership are host responsibilities.
        /// Never removes an existing endpoint to make bind succeed.
        pub fn bind(
            path: impl AsRef<Path>,
            identity: &DocumentIdentity,
            peers: Vec<PathBuf>,
        ) -> Result<Self, PortError> {
            crate::ports::scope(identity)?;
            if peers.len() > 16 {
                return Err(PortError::ResourceLimit);
            }
            let path = path.as_ref().to_path_buf();
            let socket = UnixDatagram::bind(&path).map_err(io)?;
            let inode = std::fs::symlink_metadata(&path).map_err(io)?.ino();
            let mut channel = Self {
                socket,
                path,
                inode,
                scope: [0; 32],
                peers,
            };
            channel.socket.set_nonblocking(true).map_err(io)?;
            let mut hash = Sha256::new();
            for field in [
                &identity.document_id,
                &identity.epoch,
                &identity.profile,
                &identity.schema,
                &identity.offsets,
            ] {
                hash.update((field.len() as u64).to_be_bytes());
                hash.update(field.as_bytes());
            }
            hash.update(identity.protocol_version.to_be_bytes());
            hash.update(identity.update_encoding.to_be_bytes());
            channel.scope = hash.finalize().into();
            Ok(channel)
        }
    }
    impl LocalChannel for UnixLocalChannel {
        fn notify(&mut self, writer: u32) -> Result<(), PortError> {
            if writer == 0 {
                return Err(PortError::InvalidRecord);
            }
            let mut message = [0; 36];
            message[..32].copy_from_slice(&self.scope);
            message[32..].copy_from_slice(&writer.to_be_bytes());
            // Partial broadcast remains recoverable: never delete journal/outbox on failure.
            for peer in &self.peers {
                self.socket.send_to(&message, peer).map_err(io)?;
            }
            Ok(())
        }
        fn receive(&mut self) -> Result<Vec<u32>, PortError> {
            let mut result = vec![];
            for _ in 0..64 {
                let mut message = [0; 37];
                match self.socket.recv(&mut message) {
                    Ok(36) if message[..32] == self.scope => {
                        let writer =
                            u32::from_be_bytes(message[32..36].try_into().expect("four bytes"));
                        if writer != 0 {
                            result.push(writer);
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(io(error)),
                }
            }
            Ok(result)
        }
    }
    impl Drop for UnixLocalChannel {
        fn drop(&mut self) {
            // Preserve a replacement endpoint belonging to another owner.
            if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}
