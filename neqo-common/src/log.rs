// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![allow(clippy::module_name_repetitions)]

use std::{
    io::Write,
    sync::{Once, OnceLock},
    time::{Duration, Instant},
};

use env_logger::Builder;

#[macro_export]
macro_rules! do_log {
    (target: $target:expr, $lvl:expr, $($arg:tt)+) => ({
        let lvl = $lvl;
        if lvl <= ::log::max_level() {
            ::log::logger().log(
                &::log::Record::builder()
                    .args(format_args!($($arg)+))
                    .level(lvl)
                    .target($target)
                    .module_path_static(Some(module_path!()))
                    .file_static(Some(file!()))
                    .line(Some(line!()))
                    .build()
            );
        }
    });
    ($lvl:expr, $($arg:tt)+) => ($crate::do_log!(target: module_path!(), $lvl, $($arg)+))
}

#[macro_export]
macro_rules! log_subject {
    ($lvl:expr, $subject:expr) => {{
        if $lvl <= ::log::max_level() {
            format!("{}", $subject)
        } else {
            String::new()
        }
    }};
}

fn since_start() -> Duration {
    static START_TIME: OnceLock<Instant> = OnceLock::new();
    START_TIME.get_or_init(Instant::now).elapsed()
}

pub fn init() {
    static INIT_ONCE: Once = Once::new();
    INIT_ONCE.call_once(|| {
        let mut builder = Builder::from_env("RUST_LOG");
        builder.format(|buf, record| {
            let elapsed = since_start();
            writeln!(
                buf,
                "{}s{:3}ms {} {}",
                elapsed.as_secs(),
                elapsed.as_millis() % 1000,
                record.level(),
                record.args()
            )
        });
        if let Err(e) = builder.try_init() {
            do_log!(::log::Level::Info, "Logging initialization error {:?}", e);
        } else {
            do_log!(::log::Level::Info, "Logging initialized");
        }
    });
}

#[macro_export]
macro_rules! log_invoke {
    ($lvl:expr, $ctx:expr, $($arg:tt)*) => ( {
        ::neqo_common::log::init();
        ::neqo_common::do_log!($lvl, "[{}] {}", $ctx, format!($($arg)*));
    } )
}

#[cfg(not(feature = "bench"))]
#[macro_export]
macro_rules! qerror {
    ([$ctx:expr], $($arg:tt)*) => (::neqo_common::log_invoke!(::log::Level::Error, $ctx, $($arg)*););
    ($($arg:tt)*) => ( { ::neqo_common::log::init(); ::neqo_common::do_log!(::log::Level::Error, $($arg)*); } );
}

#[cfg(feature = "bench")]
#[macro_export]
macro_rules! qerror {
    ([$ctx:expr], $($arg:tt)*) => (());
    ($($arg:tt)*) => (());
}

#[cfg(not(feature = "bench"))]
#[macro_export]
macro_rules! qwarn {
    ([$ctx:expr], $($arg:tt)*) => (::neqo_common::log_invoke!(::log::Level::Warn, $ctx, $($arg)*););
    ($($arg:tt)*) => ( { ::neqo_common::log::init(); ::neqo_common::do_log!(::log::Level::Warn, $($arg)*); } );
}

#[cfg(feature = "bench")]
#[macro_export]
macro_rules! qwarn {
    ([$ctx:expr], $($arg:tt)*) => (());
    ($($arg:tt)*) => (());
}

#[cfg(not(feature = "bench"))]
#[macro_export]
macro_rules! qinfo {
    ([$ctx:expr], $($arg:tt)*) => (::neqo_common::log_invoke!(::log::Level::Info, $ctx, $($arg)*););
    ($($arg:tt)*) => ( { ::neqo_common::log::init(); ::neqo_common::do_log!(::log::Level::Info, $($arg)*); } );
}

#[cfg(feature = "bench")]
#[macro_export]
macro_rules! qinfo {
    ([$ctx:expr], $($arg:tt)*) => (());
    ($($arg:tt)*) => (());
}

#[cfg(not(feature = "bench"))]
#[macro_export]
macro_rules! qdebug {
    ([$ctx:expr], $($arg:tt)*) => (::neqo_common::log_invoke!(::log::Level::Debug, $ctx, $($arg)*););
    ($($arg:tt)*) => ( { ::neqo_common::log::init(); ::neqo_common::do_log!(::log::Level::Debug, $($arg)*); } );
}

#[cfg(feature = "bench")]
#[macro_export]
macro_rules! qdebug {
    ([$ctx:expr], $($arg:tt)*) => (());
    ($($arg:tt)*) => (());
}

#[cfg(not(feature = "bench"))]
#[macro_export]
macro_rules! qtrace {
    ([$ctx:expr], $($arg:tt)*) => (::neqo_common::log_invoke!(::log::Level::Trace, $ctx, $($arg)*););
    ($($arg:tt)*) => ( { ::neqo_common::log::init(); ::neqo_common::do_log!(::log::Level::Trace, $($arg)*); } );
}

#[cfg(feature = "bench")]
#[macro_export]
macro_rules! qtrace {
    ([$ctx:expr], $($arg:tt)*) => (());
    ($($arg:tt)*) => (());
}
