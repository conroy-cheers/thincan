#![no_std]
#![warn(missing_docs)]

//! `embedded-can-interface` adapters for `embassy-stm32`.
//!
//! Enable the `embassy-stm32` cargo feature to get access to the wrapper types.

#[cfg(all(feature = "bxcan", feature = "fdcan"))]
compile_error!("embedded-can-embassy-stm32: features `bxcan` and `fdcan` are mutually exclusive.");

#[cfg(feature = "embassy-stm32")]
mod stm32;

#[cfg(feature = "embassy-stm32")]
pub use stm32::*;

#[cfg(all(feature = "embassy-stm32", feature = "bxcan"))]
mod bxcan;

#[cfg(all(feature = "embassy-stm32", feature = "bxcan"))]
pub use bxcan::*;

#[cfg(all(feature = "embassy-stm32", feature = "fdcan"))]
mod fdcan;

#[cfg(all(feature = "embassy-stm32", feature = "fdcan"))]
pub use fdcan::*;
