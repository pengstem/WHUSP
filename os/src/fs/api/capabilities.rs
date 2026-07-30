//! Narrow identity capabilities for special open file descriptions.

pub trait SocketFileCapability: Send + Sync {}

pub trait PipeFileCapability: Send + Sync {}

pub trait TtyFileCapability: Send + Sync {}
