pub mod config;
pub mod escape_filter;
pub mod escape_sequences;
pub mod history_filter;
pub mod key_parser;
pub mod line_buffer;
pub mod redraw_throttler;

#[cfg(unix)]
pub mod proxy;

#[cfg(windows)]
#[path = "proxy_win.rs"]
pub mod proxy;
