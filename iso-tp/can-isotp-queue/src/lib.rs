//! Queue/actor adapter for split ISO-TP async transport usage.
//!
//! This crate provides reusable transport plumbing for the common pattern where:
//! - one task owns a concrete ISO-TP transport backend,
//! - protocol code needs a cloneable TX handle and a uniquely owned RX handle,
//! - and callers want runtime-specific queue adapters without rewriting this glue.

#![cfg_attr(not(feature = "tokio"), no_std)]

pub mod common;

#[cfg(feature = "tokio")]
pub mod tokio;

#[cfg(feature = "embassy")]
pub mod embassy;

pub use common::{ActorConfig, ActorError, ActorHooks, ActorStop, NoopHooks, QueueError, RxEvent};
