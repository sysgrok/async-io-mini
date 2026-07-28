//! Async I/O for the ESP IDF (and possibly other MCU RTOSes supporting the [select] call and BSD Sockets).
//!
//! This crate provides [`Async`], an adapter for standard networking types (and [many other] types) to use in
//! async programs.
//!
//! # Implementation
//!
//! The first time [`Async`] is used, a thread called "async-io-mini" will be spawned.
//! The purpose of this thread is to wait for I/O events reported by the OS, and then
//! wake appropriate futures blocked on I/O when they can be resumed. Its stack is
//! [`REACTOR_STACK_SIZE`] bytes, selectable with the `stack-size-*` features.
//!
//! Note that "async-io-mini" is the Rust-level thread name. On the ESP IDF the name of
//! the underlying RTOS task is taken from `esp_pthread_cfg_t` rather than from the Rust
//! thread name, so the reactor shows up as "pthread" in panic reports and task dumps.
//!
//! To wait for the next I/O event, the task uses the [select] syscall available on many operating systems.
//!
//! # Examples
//!
//! Connect to `example.com:80`.
//!
//! ```
//! use async_io_mini::Async;
//!
//! use std::net::{TcpStream, ToSocketAddrs};
//!
//! # futures_lite::future::block_on(async {
//! let addr = "example.com:80".to_socket_addrs()?.next().unwrap();
//!
//! let stream = Async::<TcpStream>::connect(addr).await?;
//! # std::io::Result::Ok(()) });
//! ```

#![allow(unknown_lints)]
#![allow(clippy::needless_maybe_sized)]

use cfg_if::cfg_if;

pub use io::*;
#[cfg(feature = "embassy-time")]
pub use timer::*;

mod io;
mod reactor;
mod sys;
#[cfg(feature = "embassy-time")]
mod timer;

cfg_if! {
    if #[cfg(feature = "stack-size-8192")] {
        /// The size (in bytes) of the stack of the reactor thread.
        ///
        /// Select a different size with one of the `stack-size-*` features. Anything
        /// the reactor calls runs on this stack, including the platform `select`
        /// implementation and - on MCUs where interrupts are serviced on the stack of
        /// the interrupted task, as is the case on RISC-V - interrupt handlers.
        pub const REACTOR_STACK_SIZE: usize = 8192;
    } else if #[cfg(feature = "stack-size-7168")] {
        /// The size (in bytes) of the stack of the reactor thread.
        pub const REACTOR_STACK_SIZE: usize = 7168;
    } else if #[cfg(feature = "stack-size-6144")] {
        /// The size (in bytes) of the stack of the reactor thread.
        pub const REACTOR_STACK_SIZE: usize = 6144;
    } else if #[cfg(feature = "stack-size-5120")] {
        /// The size (in bytes) of the stack of the reactor thread.
        pub const REACTOR_STACK_SIZE: usize = 5120;
    } else if #[cfg(feature = "stack-size-3072")] {
        /// The size (in bytes) of the stack of the reactor thread.
        pub const REACTOR_STACK_SIZE: usize = 3072;
    } else if #[cfg(feature = "stack-size-2048")] {
        /// The size (in bytes) of the stack of the reactor thread.
        pub const REACTOR_STACK_SIZE: usize = 2048;
    } else { // Default (`stack-size-4096`)
        /// The size (in bytes) of the stack of the reactor thread.
        ///
        /// Select a different size with one of the `stack-size-*` features. Anything
        /// the reactor calls runs on this stack, including the platform `select`
        /// implementation and - on MCUs where interrupts are serviced on the stack of
        /// the interrupted task, as is the case on RISC-V - interrupt handlers.
        pub const REACTOR_STACK_SIZE: usize = 4096;
    }
}
