//! Windows ConPTY proxy implementation for claude-chill.
//!
//! This is the Windows equivalent of proxy.rs (Unix). It uses ConPTY
//! for pseudo-terminal support and threads for I/O multiplexing.

#![allow(non_snake_case, non_camel_case_types)]

use crate::escape_sequences::{
    ALT_SCREEN_ENTER, ALT_SCREEN_ENTER_LEGACY, ALT_SCREEN_EXIT, ALT_SCREEN_EXIT_LEGACY,
    BRACKETED_PASTE_DISABLE, BRACKETED_PASTE_ENABLE, BRACKETED_PASTE_END, BRACKETED_PASTE_START,
    CLEAR_SCREEN, CURSOR_HOME, INPUT_BUFFER_CAPACITY, OUTPUT_BUFFER_CAPACITY, SYNC_BUFFER_CAPACITY,
    SYNC_END, SYNC_START,
};
use crate::history_filter::HistoryFilter;
use crate::line_buffer::LineBuffer;
use anyhow::{Context, Result, bail};
use log::debug;
use memchr::memmem;
use std::ffi::c_void;
use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use termwiz::escape::Action;
use termwiz::escape::csi::{CSI, Keyboard};
use termwiz::escape::parser::Parser as TermwizParser;

// ---------------------------------------------------------------------------
// Windows FFI declarations
// ---------------------------------------------------------------------------

type HANDLE = *mut c_void;
type HPCON = *mut c_void;
type BOOL = i32;
type DWORD = u32;
type HRESULT = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct COORD {
    X: i16,
    Y: i16,
}

#[repr(C)]
struct SMALL_RECT {
    Left: i16,
    Top: i16,
    Right: i16,
    Bottom: i16,
}

#[repr(C)]
struct CONSOLE_SCREEN_BUFFER_INFO {
    dwSize: COORD,
    dwCursorPosition: COORD,
    wAttributes: u16,
    srWindow: SMALL_RECT,
    dwMaximumWindowSize: COORD,
}

#[repr(C)]
struct SECURITY_ATTRIBUTES {
    nLength: DWORD,
    lpSecurityDescriptor: *mut c_void,
    bInheritHandle: BOOL,
}

#[repr(C)]
#[allow(dead_code)]
struct STARTUPINFOW {
    cb: DWORD,
    lpReserved: *mut u16,
    lpDesktop: *mut u16,
    lpTitle: *mut u16,
    dwX: DWORD,
    dwY: DWORD,
    dwXSize: DWORD,
    dwYSize: DWORD,
    dwXCountChars: DWORD,
    dwYCountChars: DWORD,
    dwFillAttribute: DWORD,
    dwFlags: DWORD,
    wShowWindow: u16,
    cbReserved2: u16,
    lpReserved2: *mut u8,
    hStdInput: HANDLE,
    hStdOutput: HANDLE,
    hStdError: HANDLE,
}

#[repr(C)]
struct STARTUPINFOEXW {
    StartupInfo: STARTUPINFOW,
    lpAttributeList: *mut c_void,
}

#[repr(C)]
struct PROCESS_INFORMATION {
    hProcess: HANDLE,
    hThread: HANDLE,
    dwProcessId: DWORD,
    dwThreadId: DWORD,
}

const STD_INPUT_HANDLE: DWORD = 0xFFFFFFF6;
const STD_OUTPUT_HANDLE: DWORD = 0xFFFFFFF5;
const ENABLE_VIRTUAL_TERMINAL_INPUT: DWORD = 0x0200;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: DWORD = 0x0004;
const DISABLE_NEWLINE_AUTO_RETURN: DWORD = 0x0008;
const EXTENDED_STARTUPINFO_PRESENT: DWORD = 0x00080000;
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;
const INFINITE_WAIT: DWORD = 0xFFFFFFFF;

unsafe extern "system" {
    fn GetStdHandle(nStdHandle: DWORD) -> HANDLE;
    fn GetConsoleMode(hConsoleHandle: HANDLE, lpMode: *mut DWORD) -> BOOL;
    fn SetConsoleMode(hConsoleHandle: HANDLE, dwMode: DWORD) -> BOOL;
    fn GetConsoleScreenBufferInfo(
        hConsoleOutput: HANDLE,
        lpConsoleScreenBufferInfo: *mut CONSOLE_SCREEN_BUFFER_INFO,
    ) -> BOOL;
    fn SetConsoleCtrlHandler(
        HandlerRoutine: Option<unsafe extern "system" fn(DWORD) -> BOOL>,
        Add: BOOL,
    ) -> BOOL;

    fn CreatePipe(
        hReadPipe: *mut HANDLE,
        hWritePipe: *mut HANDLE,
        lpPipeAttributes: *const SECURITY_ATTRIBUTES,
        nSize: DWORD,
    ) -> BOOL;
    fn ReadFile(
        hFile: HANDLE,
        lpBuffer: *mut u8,
        nNumberOfBytesToRead: DWORD,
        lpNumberOfBytesRead: *mut DWORD,
        lpOverlapped: *mut c_void,
    ) -> BOOL;
    fn WriteFile(
        hFile: HANDLE,
        lpBuffer: *const u8,
        nNumberOfBytesToWrite: DWORD,
        lpNumberOfBytesWritten: *mut DWORD,
        lpOverlapped: *mut c_void,
    ) -> BOOL;
    fn CloseHandle(hObject: HANDLE) -> BOOL;

    fn CreatePseudoConsole(
        size: COORD,
        hInput: HANDLE,
        hOutput: HANDLE,
        dwFlags: DWORD,
        phPC: *mut HPCON,
    ) -> HRESULT;
    fn ClosePseudoConsole(hPC: HPCON);
    fn ResizePseudoConsole(hPC: HPCON, size: COORD) -> HRESULT;

    fn InitializeProcThreadAttributeList(
        lpAttributeList: *mut c_void,
        dwAttributeCount: DWORD,
        dwFlags: DWORD,
        lpSize: *mut usize,
    ) -> BOOL;
    fn UpdateProcThreadAttribute(
        lpAttributeList: *mut c_void,
        dwFlags: DWORD,
        Attribute: usize,
        lpValue: *const c_void,
        cbSize: usize,
        lpPreviousValue: *mut c_void,
        lpReturnSize: *mut usize,
    ) -> BOOL;
    fn DeleteProcThreadAttributeList(lpAttributeList: *mut c_void);

    fn CreateProcessW(
        lpApplicationName: *const u16,
        lpCommandLine: *mut u16,
        lpProcessAttributes: *const SECURITY_ATTRIBUTES,
        lpThreadAttributes: *const SECURITY_ATTRIBUTES,
        bInheritHandles: BOOL,
        dwCreationFlags: DWORD,
        lpEnvironment: *const c_void,
        lpCurrentDirectory: *const u16,
        lpStartupInfo: *const STARTUPINFOW,
        lpProcessInformation: *mut PROCESS_INFORMATION,
    ) -> BOOL;

    fn WaitForSingleObject(hHandle: HANDLE, dwMilliseconds: DWORD) -> DWORD;
    fn GetExitCodeProcess(hProcess: HANDLE, lpExitCode: *mut DWORD) -> BOOL;
    fn GetLastError() -> DWORD;
}

// ---------------------------------------------------------------------------
// Thread-safe handle wrapper
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

// ---------------------------------------------------------------------------
// Sequence matching (same as Unix)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceMatch {
    Complete,
    Partial,
    None,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const RENDER_DELAY_MS: u64 = 5;
const SYNC_BLOCK_DELAY_MS: u64 = 50;

// ---------------------------------------------------------------------------
// ProxyConfig (same interface as Unix)
// ---------------------------------------------------------------------------

pub struct ProxyConfig {
    pub max_history_lines: usize,
    pub lookback_key: String,
    pub lookback_sequence_legacy: Vec<u8>,
    pub lookback_sequence_kitty: Vec<u8>,
    pub auto_lookback_timeout_ms: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_history_lines: 100_000,
            lookback_key: "[ctrl][6]".to_string(),
            lookback_sequence_legacy: vec![0x1E],
            lookback_sequence_kitty: b"\x1b[54;5u".to_vec(),
            auto_lookback_timeout_ms: 15000,
        }
    }
}

// ---------------------------------------------------------------------------
// Console Ctrl handler — prevent default Ctrl+C termination
// ---------------------------------------------------------------------------

unsafe extern "system" fn ctrl_handler(_ctrl_type: DWORD) -> BOOL {
    1 // handled — don't terminate
}

// ---------------------------------------------------------------------------
// Console helpers
// ---------------------------------------------------------------------------

fn setup_raw_mode() -> Result<(DWORD, DWORD)> {
    unsafe {
        let stdin = GetStdHandle(STD_INPUT_HANDLE);
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);

        let mut old_stdin_mode: DWORD = 0;
        if GetConsoleMode(stdin, &mut old_stdin_mode) == 0 {
            bail!("GetConsoleMode(stdin) failed: error {}", GetLastError());
        }

        let mut old_stdout_mode: DWORD = 0;
        if GetConsoleMode(stdout, &mut old_stdout_mode) == 0 {
            bail!("GetConsoleMode(stdout) failed: error {}", GetLastError());
        }

        // Raw VT input: clears ENABLE_LINE_INPUT, ENABLE_ECHO_INPUT,
        // ENABLE_PROCESSED_INPUT. Ctrl+C arrives as byte 0x03.
        if SetConsoleMode(stdin, ENABLE_VIRTUAL_TERMINAL_INPUT) == 0 {
            bail!("SetConsoleMode(stdin) failed: error {}", GetLastError());
        }

        // Enable VT output processing
        let new_stdout_mode =
            old_stdout_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;
        if SetConsoleMode(stdout, new_stdout_mode) == 0 {
            bail!("SetConsoleMode(stdout) failed: error {}", GetLastError());
        }

        // Prevent default Ctrl+C / Ctrl+Break handler from killing us
        SetConsoleCtrlHandler(Some(ctrl_handler), 1);

        Ok((old_stdin_mode, old_stdout_mode))
    }
}

fn get_terminal_size() -> (u16, u16) {
    unsafe {
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(stdout, &mut info) != 0 {
            let cols = (info.srWindow.Right - info.srWindow.Left + 1) as u16;
            let rows = (info.srWindow.Bottom - info.srWindow.Top + 1) as u16;
            (cols.max(1), rows.max(1))
        } else {
            (80, 24)
        }
    }
}

// ---------------------------------------------------------------------------
// ConPTY helpers
// ---------------------------------------------------------------------------

/// Create a ConPTY with the given size.
/// Returns (hpc, input_write_pipe, output_read_pipe).
fn create_conpty(cols: u16, rows: u16) -> Result<(HPCON, HANDLE, HANDLE)> {
    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };

    unsafe {
        let mut input_read: HANDLE = std::ptr::null_mut();
        let mut input_write: HANDLE = std::ptr::null_mut();
        let mut output_read: HANDLE = std::ptr::null_mut();
        let mut output_write: HANDLE = std::ptr::null_mut();

        if CreatePipe(&mut input_read, &mut input_write, std::ptr::null(), 0) == 0 {
            bail!("CreatePipe(input) failed: error {}", GetLastError());
        }
        if CreatePipe(&mut output_read, &mut output_write, std::ptr::null(), 0) == 0 {
            CloseHandle(input_read);
            CloseHandle(input_write);
            bail!("CreatePipe(output) failed: error {}", GetLastError());
        }

        let mut hpc: HPCON = std::ptr::null_mut();
        let hr = CreatePseudoConsole(size, input_read, output_write, 0, &mut hpc);
        if hr != 0 {
            CloseHandle(input_read);
            CloseHandle(input_write);
            CloseHandle(output_read);
            CloseHandle(output_write);
            bail!("CreatePseudoConsole failed: HRESULT 0x{:08X}", hr);
        }

        // ConPTY now owns these ends of the pipes
        CloseHandle(input_read);
        CloseHandle(output_write);

        Ok((hpc, input_write, output_read))
    }
}

// ---------------------------------------------------------------------------
// Command resolution — find .exe / .cmd / .bat on PATH
// ---------------------------------------------------------------------------

fn resolve_command(command: &str) -> String {
    if command.contains('\\') || command.contains('/') || command.contains('.') {
        return command.to_string();
    }

    let path_var = std::env::var("PATH").unwrap_or_default();
    let pathext = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let extensions: Vec<&str> = pathext.split(';').collect();

    for dir in path_var.split(';') {
        if dir.is_empty() {
            continue;
        }
        for ext in &extensions {
            let candidate = format!("{}\\{}{}", dir, command, ext.to_lowercase());
            if std::path::Path::new(&candidate).exists() {
                return candidate;
            }
            // Also try original case
            let candidate_orig = format!("{}\\{}{}", dir, command, ext);
            if std::path::Path::new(&candidate_orig).exists() {
                return candidate_orig;
            }
        }
    }

    command.to_string()
}

fn build_command_line(command: &str, args: &[&str]) -> String {
    let resolved = resolve_command(command);
    let lower = resolved.to_lowercase();
    let is_batch = lower.ends_with(".cmd") || lower.ends_with(".bat");

    let mut cmd = if is_batch {
        format!("cmd.exe /c \"{}\"", resolved)
    } else {
        format!("\"{}\"", resolved)
    };

    for arg in args {
        cmd.push(' ');
        if arg.contains(' ') || arg.contains('"') {
            cmd.push('"');
            cmd.push_str(arg);
            cmd.push('"');
        } else {
            cmd.push_str(arg);
        }
    }

    cmd
}

// ---------------------------------------------------------------------------
// Process spawning with ConPTY
// ---------------------------------------------------------------------------

fn spawn_process(hpc: HPCON, command_line: &str) -> Result<(HANDLE, HANDLE)> {
    unsafe {
        // Determine attribute list size
        let mut attr_size: usize = 0;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);

        let mut attr_buf = vec![0u8; attr_size];
        let attr_list = attr_buf.as_mut_ptr() as *mut c_void;

        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
            bail!(
                "InitializeProcThreadAttributeList failed: error {}",
                GetLastError()
            );
        }

        if UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            hpc,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == 0
        {
            DeleteProcThreadAttributeList(attr_list);
            bail!("UpdateProcThreadAttribute failed: error {}", GetLastError());
        }

        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as DWORD;
        si.lpAttributeList = attr_list;

        let mut cmd_wide: Vec<u16> = command_line
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();

        let result = CreateProcessW(
            std::ptr::null(),
            cmd_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // don't inherit handles
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        );

        DeleteProcThreadAttributeList(attr_list);

        if result == 0 {
            let err = GetLastError();
            bail!(
                "CreateProcessW failed: error {} — command: {}",
                err,
                command_line
            );
        }

        Ok((pi.hProcess, pi.hThread))
    }
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

pub struct Proxy {
    config: ProxyConfig,
    hpc: HPCON,
    pty_input: SendHandle,
    #[allow(dead_code)]
    pty_output: SendHandle,
    process_handle: HANDLE,
    thread_handle: HANDLE,
    original_stdin_mode: DWORD,
    original_stdout_mode: DWORD,
    last_cols: u16,
    last_rows: u16,

    // Shared rendering / history state
    history: LineBuffer,
    history_filter: HistoryFilter,
    vt_parser: vt100::Parser,
    vt_prev_screen: Option<vt100::Screen>,
    last_output_time: Option<Instant>,
    last_render_time: Option<Instant>,
    last_stdin_time: Option<Instant>,
    last_auto_lookback_time: Option<Instant>,
    auto_lookback_timeout: Duration,
    sync_buffer: Vec<u8>,
    in_sync_block: bool,
    in_lookback_mode: bool,
    in_alternate_screen: bool,
    in_bracketed_paste: bool,
    kitty_mode_supported: bool,
    kitty_mode_stack: u32,
    kitty_output_parser: TermwizParser,
    vt_render_pending: bool,
    lookback_cache: Vec<u8>,
    lookback_input_buffer: Vec<u8>,
    output_buffer: Vec<u8>,
    sync_start_finder: memmem::Finder<'static>,
    sync_end_finder: memmem::Finder<'static>,
    clear_screen_finder: memmem::Finder<'static>,
    cursor_home_finder: memmem::Finder<'static>,
    alt_screen_enter_finder: memmem::Finder<'static>,
    alt_screen_exit_finder: memmem::Finder<'static>,
    alt_screen_enter_legacy_finder: memmem::Finder<'static>,
    alt_screen_exit_legacy_finder: memmem::Finder<'static>,
    paste_start_finder: memmem::Finder<'static>,
    paste_end_finder: memmem::Finder<'static>,
    paste_remainder: Vec<u8>,
}

impl Proxy {
    pub fn spawn(command: &str, args: &[&str], config: ProxyConfig) -> Result<Self> {
        let (cols, rows) = get_terminal_size();
        let (hpc, pty_input, pty_output) = create_conpty(cols, rows)?;

        let (original_stdin_mode, original_stdout_mode) = setup_raw_mode()?;

        let cmd_line = build_command_line(command, args);
        debug!("Proxy::spawn: command_line={}", cmd_line);

        let (process_handle, thread_handle) =
            spawn_process(hpc, &cmd_line).context("spawn failed")?;

        // Enable bracketed paste on the real terminal
        {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(BRACKETED_PASTE_ENABLE);
            let _ = stdout.flush();
        }

        let vt_parser = vt100::Parser::new(rows, cols, 0);
        let mut history = LineBuffer::new(config.max_history_lines);
        history.push_bytes(CLEAR_SCREEN);
        history.push_bytes(CURSOR_HOME);

        let auto_lookback_timeout = Duration::from_millis(config.auto_lookback_timeout_ms);

        Ok(Self {
            history,
            history_filter: HistoryFilter::new(),
            config,
            hpc,
            pty_input: SendHandle(pty_input),
            pty_output: SendHandle(pty_output),
            process_handle,
            thread_handle,
            original_stdin_mode,
            original_stdout_mode,
            last_cols: cols,
            last_rows: rows,
            vt_parser,
            vt_prev_screen: None,
            last_output_time: None,
            last_render_time: None,
            last_stdin_time: None,
            last_auto_lookback_time: None,
            auto_lookback_timeout,
            sync_buffer: Vec::with_capacity(SYNC_BUFFER_CAPACITY),
            in_sync_block: false,
            in_lookback_mode: false,
            in_alternate_screen: false,
            in_bracketed_paste: false,
            kitty_mode_supported: false,
            kitty_mode_stack: 0,
            kitty_output_parser: TermwizParser::new(),
            vt_render_pending: false,
            lookback_cache: Vec::new(),
            lookback_input_buffer: Vec::with_capacity(INPUT_BUFFER_CAPACITY),
            output_buffer: Vec::with_capacity(OUTPUT_BUFFER_CAPACITY),
            sync_start_finder: memmem::Finder::new(SYNC_START),
            sync_end_finder: memmem::Finder::new(SYNC_END),
            clear_screen_finder: memmem::Finder::new(CLEAR_SCREEN),
            cursor_home_finder: memmem::Finder::new(CURSOR_HOME),
            alt_screen_enter_finder: memmem::Finder::new(ALT_SCREEN_ENTER),
            alt_screen_exit_finder: memmem::Finder::new(ALT_SCREEN_EXIT),
            alt_screen_enter_legacy_finder: memmem::Finder::new(ALT_SCREEN_ENTER_LEGACY),
            alt_screen_exit_legacy_finder: memmem::Finder::new(ALT_SCREEN_EXIT_LEGACY),
            paste_start_finder: memmem::Finder::new(BRACKETED_PASTE_START),
            paste_end_finder: memmem::Finder::new(BRACKETED_PASTE_END),
            paste_remainder: Vec::new(),
        })
    }

    // -----------------------------------------------------------------------
    // Main event loop (thread-based I/O)
    // -----------------------------------------------------------------------

    pub fn run(&mut self) -> Result<i32> {
        enum Event {
            PtyOutput(Vec<u8>),
            StdinInput(Vec<u8>),
            PtyEof,
        }

        let (tx, rx) = mpsc::channel::<Event>();

        // Thread: read ConPTY output
        // Cast handle to usize to safely send across thread boundary
        let pty_out_raw = self.pty_output.0 as usize;
        let tx_pty = tx.clone();
        thread::spawn(move || {
            let handle = pty_out_raw as HANDLE;
            let mut buf = [0u8; 65536];
            loop {
                let mut bytes_read: DWORD = 0;
                let ok = unsafe {
                    ReadFile(
                        handle,
                        buf.as_mut_ptr(),
                        buf.len() as DWORD,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || bytes_read == 0 {
                    let _ = tx_pty.send(Event::PtyEof);
                    break;
                }
                if tx_pty
                    .send(Event::PtyOutput(buf[..bytes_read as usize].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
        });

        // Thread: read console stdin (VT input mode → raw bytes)
        let tx_stdin = tx.clone();
        thread::spawn(move || {
            let stdin_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            let mut buf = [0u8; 4096];
            loop {
                let mut bytes_read: DWORD = 0;
                let ok = unsafe {
                    ReadFile(
                        stdin_handle,
                        buf.as_mut_ptr(),
                        buf.len() as DWORD,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || bytes_read == 0 {
                    break;
                }
                if tx_stdin
                    .send(Event::StdinInput(buf[..bytes_read as usize].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
        });

        drop(tx); // so rx disconnects when both threads exit

        loop {
            let poll_timeout = self
                .time_until_render()
                .map(|d| d.as_millis().min(100) as u64)
                .unwrap_or(100);

            match rx.recv_timeout(Duration::from_millis(poll_timeout)) {
                Ok(Event::PtyOutput(data)) => {
                    self.process_output(&data)?;
                }
                Ok(Event::StdinInput(data)) => {
                    self.process_input(&data)?;
                }
                Ok(Event::PtyEof) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.flush_pending_vt_render()?;
                    self.check_auto_lookback()?;
                    self.check_resize()?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        if self.vt_render_pending {
            self.render_vt_screen()?;
        }

        self.wait_child()
    }

    // -----------------------------------------------------------------------
    // I/O helpers
    // -----------------------------------------------------------------------

    fn write_stdout(data: &[u8]) -> Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(data).context("write to stdout failed")?;
        stdout.flush()?;
        Ok(())
    }

    fn write_pty(&self, data: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let mut written: DWORD = 0;
            let ok = unsafe {
                WriteFile(
                    self.pty_input.0,
                    data[offset..].as_ptr(),
                    (data.len() - offset) as DWORD,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                bail!("WriteFile to PTY failed: error {}", unsafe {
                    GetLastError()
                });
            }
            offset += written as usize;
        }
        Ok(())
    }

    fn write_to_terminal(&mut self, data: &[u8]) -> Result<()> {
        Self::update_kitty_mode_helper(
            &mut self.kitty_output_parser,
            &mut self.kitty_mode_stack,
            self.kitty_mode_supported,
            data,
        );
        Self::write_stdout(data)
    }

    // -----------------------------------------------------------------------
    // Output processing (adapted from Unix proxy.rs)
    // -----------------------------------------------------------------------

    fn process_output(&mut self, data: &[u8]) -> Result<()> {
        self.process_output_inner(data, true)
    }

    fn process_output_inner(&mut self, data: &[u8], feed_vt: bool) -> Result<()> {
        debug!(
            "process_output: len={} in_alt={} in_lookback={} feed_vt={}",
            data.len(),
            self.in_alternate_screen,
            self.in_lookback_mode,
            feed_vt
        );

        if self.in_alternate_screen {
            if feed_vt {
                self.vt_parser.process(data);
            }
            return self.process_output_alt_screen(data);
        }

        if self.in_lookback_mode {
            debug!("process_output: caching {} bytes for lookback", data.len());
            self.lookback_cache.extend_from_slice(data);
            return Ok(());
        }

        if feed_vt {
            self.vt_parser.process(data);
        }
        self.vt_render_pending = true;
        self.last_output_time = Some(Instant::now());

        let mut pos = 0;
        while pos < data.len() {
            if let Some(alt_pos) = self.find_alt_screen_enter(&data[pos..]) {
                debug!(
                    "process_output: ALT_SCREEN_ENTER detected at pos={}",
                    pos + alt_pos
                );
                let remaining = &data[pos..];
                if self.in_sync_block {
                    self.sync_buffer.extend_from_slice(remaining);
                    self.flush_sync_block_to_history();
                    self.in_sync_block = false;
                } else {
                    self.push_to_history(remaining);
                }
                self.in_alternate_screen = true;
                let seq_len = self.alt_screen_enter_len(&data[pos + alt_pos..]);
                self.write_to_terminal(&data[pos + alt_pos..pos + alt_pos + seq_len])?;
                return self.process_output_alt_screen(&data[pos + alt_pos + seq_len..]);
            }

            if self.in_sync_block {
                if let Some(idx) = self.sync_end_finder.find(&data[pos..]) {
                    debug!("process_output: SYNC_END at pos={}", pos + idx);
                    self.sync_buffer.extend_from_slice(&data[pos..pos + idx]);
                    self.sync_buffer.extend_from_slice(SYNC_END);
                    self.flush_sync_block_to_history();
                    self.in_sync_block = false;
                    pos += idx + SYNC_END.len();
                } else {
                    self.sync_buffer.extend_from_slice(&data[pos..]);
                    break;
                }
            } else if let Some(idx) = self.sync_start_finder.find(&data[pos..]) {
                debug!("process_output: SYNC_START at pos={}", pos + idx);
                if idx > 0 {
                    self.push_to_history(&data[pos..pos + idx]);
                }
                self.in_sync_block = true;
                self.sync_buffer.clear();
                self.sync_buffer.extend_from_slice(SYNC_START);
                pos += idx + SYNC_START.len();
            } else {
                self.push_to_history(&data[pos..]);
                break;
            }
        }

        Ok(())
    }

    fn process_output_alt_screen(&mut self, data: &[u8]) -> Result<()> {
        if let Some(exit_pos) = self.find_alt_screen_exit(data) {
            debug!(
                "process_output_alt_screen: ALT_SCREEN_EXIT at pos={}",
                exit_pos
            );
            self.write_to_terminal(&data[..exit_pos])?;
            let seq_len = self.alt_screen_exit_len(&data[exit_pos..]);
            self.write_to_terminal(&data[exit_pos..exit_pos + seq_len])?;
            self.in_alternate_screen = false;

            debug!("process_output_alt_screen: rendering VT screen after alt exit");
            self.vt_prev_screen = None;
            self.render_vt_screen()?;

            let remaining = &data[exit_pos + seq_len..];
            if !remaining.is_empty() && self.find_alt_screen_enter(remaining).is_some() {
                return self.process_output_check_alt_only(remaining);
            }
            return Ok(());
        }
        self.write_to_terminal(data)
    }

    fn process_output_check_alt_only(&mut self, data: &[u8]) -> Result<()> {
        if let Some(alt_pos) = self.find_alt_screen_enter(data) {
            debug!(
                "process_output_check_alt_only: ALT_SCREEN_ENTER at pos={}",
                alt_pos
            );
            self.in_alternate_screen = true;
            let seq_len = self.alt_screen_enter_len(&data[alt_pos..]);
            self.write_to_terminal(&data[alt_pos..alt_pos + seq_len])?;
            return self.process_output_alt_screen(&data[alt_pos + seq_len..]);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Alt-screen finders (identical to Unix)
    // -----------------------------------------------------------------------

    fn find_alt_screen_enter(&self, data: &[u8]) -> Option<usize> {
        let pos1 = self.alt_screen_enter_finder.find(data);
        let pos2 = self.alt_screen_enter_legacy_finder.find(data);
        match (pos1, pos2) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn find_alt_screen_exit(&self, data: &[u8]) -> Option<usize> {
        let pos1 = self.alt_screen_exit_finder.find(data);
        let pos2 = self.alt_screen_exit_legacy_finder.find(data);
        match (pos1, pos2) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn alt_screen_enter_len(&self, data: &[u8]) -> usize {
        if data.starts_with(ALT_SCREEN_ENTER) {
            ALT_SCREEN_ENTER.len()
        } else {
            ALT_SCREEN_ENTER_LEGACY.len()
        }
    }

    fn alt_screen_exit_len(&self, data: &[u8]) -> usize {
        if data.starts_with(ALT_SCREEN_EXIT) {
            ALT_SCREEN_EXIT.len()
        } else {
            ALT_SCREEN_EXIT_LEGACY.len()
        }
    }

    // -----------------------------------------------------------------------
    // Kitty keyboard protocol tracking (identical to Unix)
    // -----------------------------------------------------------------------

    fn kitty_mode_enabled(&self) -> bool {
        self.kitty_mode_stack > 0
    }

    fn update_kitty_mode_helper(
        parser: &mut TermwizParser,
        stack: &mut u32,
        supported: bool,
        data: &[u8],
    ) {
        let actions = parser.parse_as_vec(data);
        for action in actions {
            if let Action::CSI(csi) = action {
                match csi {
                    CSI::Keyboard(Keyboard::PushKittyState { flags, .. }) => {
                        if supported {
                            *stack = stack.saturating_add(1);
                            debug!("Kitty push (flags={:?}, stack={})", flags, stack);
                        }
                    }
                    CSI::Keyboard(Keyboard::SetKittyState { flags, .. }) => {
                        if supported && !flags.is_empty() && *stack == 0 {
                            *stack = 1;
                            debug!("Kitty set (flags={:?}, stack={})", flags, stack);
                        }
                    }
                    CSI::Keyboard(Keyboard::PopKittyState(n)) => {
                        let prev = *stack;
                        *stack = stack.saturating_sub(n);
                        debug!("Kitty pop {} (stack {} -> {})", n, prev, stack);
                    }
                    _ => {}
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // History management (identical to Unix)
    // -----------------------------------------------------------------------

    fn flush_sync_block_to_history(&mut self) {
        let has_clear_screen = self.clear_screen_finder.find(&self.sync_buffer).is_some();
        let has_cursor_home = self.cursor_home_finder.find(&self.sync_buffer).is_some();
        let is_full_redraw = has_clear_screen && has_cursor_home;

        debug!(
            "flush_sync_block: len={} full_redraw={}",
            self.sync_buffer.len(),
            is_full_redraw
        );

        if is_full_redraw {
            debug!("CLEARING HISTORY");
            self.history.clear();
            self.history.push_bytes(CLEAR_SCREEN);
            self.history.push_bytes(CURSOR_HOME);
        }
        self.push_to_history(&self.sync_buffer.clone());
        self.sync_buffer.clear();
    }

    fn push_to_history(&mut self, data: &[u8]) {
        let filtered = self.history_filter.filter(data);
        self.history.push_bytes(&filtered);
    }

    // -----------------------------------------------------------------------
    // VT rendering (adapted from Unix — writes to stdout directly)
    // -----------------------------------------------------------------------

    fn flush_pending_vt_render(&mut self) -> Result<()> {
        if !self.vt_render_pending || self.in_lookback_mode || self.in_alternate_screen {
            return Ok(());
        }

        let elapsed = self
            .last_output_time
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);

        let delay = if self.in_sync_block {
            Duration::from_millis(SYNC_BLOCK_DELAY_MS)
        } else {
            Duration::from_millis(RENDER_DELAY_MS)
        };

        if elapsed >= delay {
            self.render_vt_screen()?;
        }

        Ok(())
    }

    fn time_until_render(&self) -> Option<Duration> {
        if !self.vt_render_pending || self.in_lookback_mode || self.in_alternate_screen {
            return None;
        }

        let elapsed = self
            .last_output_time
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);

        let delay = if self.in_sync_block {
            Duration::from_millis(SYNC_BLOCK_DELAY_MS)
        } else {
            Duration::from_millis(RENDER_DELAY_MS)
        };

        if elapsed >= delay {
            Some(Duration::ZERO)
        } else {
            Some(delay - elapsed)
        }
    }

    fn render_vt_screen(&mut self) -> Result<()> {
        let is_diff = self.vt_prev_screen.is_some();
        self.output_buffer.clear();
        self.output_buffer.extend_from_slice(SYNC_START);

        match &self.vt_prev_screen {
            Some(prev) => {
                self.output_buffer
                    .extend_from_slice(&self.vt_parser.screen().contents_diff(prev));
            }
            None => {
                self.output_buffer
                    .extend_from_slice(&self.vt_parser.screen().contents_formatted());
            }
        }

        self.output_buffer
            .extend_from_slice(&self.vt_parser.screen().cursor_state_formatted());
        self.output_buffer.extend_from_slice(SYNC_END);

        debug!(
            "render_vt_screen: diff={} output_len={}",
            is_diff,
            self.output_buffer.len()
        );

        Self::update_kitty_mode_helper(
            &mut self.kitty_output_parser,
            &mut self.kitty_mode_stack,
            self.kitty_mode_supported,
            &self.output_buffer,
        );
        Self::write_stdout(&self.output_buffer)?;

        self.vt_prev_screen = Some(self.vt_parser.screen().clone());
        self.vt_render_pending = false;
        self.last_render_time = Some(Instant::now());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Auto-lookback (identical logic to Unix)
    // -----------------------------------------------------------------------

    fn check_auto_lookback(&mut self) -> Result<()> {
        if self.auto_lookback_timeout.is_zero() {
            return Ok(());
        }
        if self.in_lookback_mode || self.in_alternate_screen {
            return Ok(());
        }

        let Some(stdin_time) = self.last_stdin_time else {
            return Ok(());
        };
        if stdin_time.elapsed() < self.auto_lookback_timeout {
            return Ok(());
        }

        let Some(render_time) = self.last_render_time else {
            return Ok(());
        };
        if let Some(last_auto) = self.last_auto_lookback_time {
            let no_new_output = render_time <= last_auto;
            let too_soon = last_auto.elapsed() < self.auto_lookback_timeout;
            if no_new_output || too_soon {
                return Ok(());
            }
        }

        debug!("auto_lookback triggered");
        self.dump_history()?;
        self.last_auto_lookback_time = Some(Instant::now());
        Ok(())
    }

    fn dump_history(&mut self) -> Result<()> {
        debug!(
            "dump_history: history_bytes={} lines={}",
            self.history.total_bytes(),
            self.history.line_count()
        );
        self.output_buffer.clear();
        self.history.append_all(&mut self.output_buffer);

        if let Ok(path) = std::env::var("CLAUDE_CHILL_HISTORY_FILE") {
            if let Err(e) = std::fs::write(&path, &self.output_buffer) {
                debug!("Failed to write history file: {}", e);
            }
        }

        self.write_to_terminal(CLEAR_SCREEN)?;
        self.write_to_terminal(CURSOR_HOME)?;

        Self::update_kitty_mode_helper(
            &mut self.kitty_output_parser,
            &mut self.kitty_mode_stack,
            self.kitty_mode_supported,
            &self.output_buffer,
        );
        Self::write_stdout(&self.output_buffer)?;

        self.vt_prev_screen = None;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Input processing (adapted from Unix)
    // -----------------------------------------------------------------------

    fn process_input(&mut self, data: &[u8]) -> Result<()> {
        self.last_stdin_time = Some(Instant::now());

        debug!(
            "process_input: len={} paste={}",
            data.len(),
            self.in_bracketed_paste
        );

        if self.in_alternate_screen {
            return self.write_pty(data);
        }

        if self.in_bracketed_paste {
            let combined;
            let search_data = if self.paste_remainder.is_empty() {
                data
            } else {
                combined = [self.paste_remainder.as_slice(), data].concat();
                self.paste_remainder.clear();
                &combined
            };

            if let Some(pos) = self.paste_end_finder.find(search_data) {
                let end = pos + BRACKETED_PASTE_END.len();
                self.write_pty(&search_data[..end])?;
                self.in_bracketed_paste = false;
                debug!("process_input: bracketed paste ended");
                if end < search_data.len() {
                    return self.process_input(&search_data[end..]);
                }
                return Ok(());
            }

            let (forward, remainder) =
                split_trailing_marker_prefix(search_data, BRACKETED_PASTE_END);
            if !forward.is_empty() {
                self.write_pty(forward)?;
            }
            self.paste_remainder.extend_from_slice(remainder);
            return Ok(());
        }

        if let Some(pos) = self.paste_start_finder.find(data) {
            debug!("process_input: bracketed paste started");
            if pos > 0 {
                self.process_input_lookback(&data[..pos])?;
            }
            self.in_bracketed_paste = true;
            let paste_data = &data[pos..];
            let search_start = BRACKETED_PASTE_START.len();
            if paste_data.len() > search_start {
                if let Some(end_pos) = self.paste_end_finder.find(&paste_data[search_start..]) {
                    let end = search_start + end_pos + BRACKETED_PASTE_END.len();
                    self.write_pty(&paste_data[..end])?;
                    self.in_bracketed_paste = false;
                    debug!("process_input: bracketed paste ended (same chunk)");
                    if end < paste_data.len() {
                        return self.process_input(&paste_data[end..]);
                    }
                    return Ok(());
                }
            }
            let (forward, remainder) =
                split_trailing_marker_prefix(paste_data, BRACKETED_PASTE_END);
            if !forward.is_empty() {
                self.write_pty(forward)?;
            }
            self.paste_remainder.extend_from_slice(remainder);
            return Ok(());
        }

        self.process_input_lookback(data)
    }

    fn process_input_lookback(&mut self, data: &[u8]) -> Result<()> {
        let lookback_sequence = if self.kitty_mode_enabled() {
            self.config.lookback_sequence_kitty.clone()
        } else {
            self.config.lookback_sequence_legacy.clone()
        };

        for &byte in data {
            if self.in_lookback_mode && byte == 0x03 {
                self.lookback_input_buffer.clear();
                self.exit_lookback_mode()?;
                continue;
            }

            let lookback_action = self.check_sequence_match(
                byte,
                &mut self.lookback_input_buffer.clone(),
                &lookback_sequence,
            );

            self.lookback_input_buffer.push(byte);

            if self.lookback_input_buffer.len() > lookback_sequence.len() {
                let excess = self.lookback_input_buffer.len() - lookback_sequence.len();
                self.lookback_input_buffer.drain(..excess);
            }

            match lookback_action {
                SequenceMatch::Complete => {
                    self.lookback_input_buffer.clear();
                    if self.in_lookback_mode {
                        self.exit_lookback_mode()?;
                    } else {
                        self.enter_lookback_mode()?;
                    }
                    continue;
                }
                SequenceMatch::Partial => {
                    continue;
                }
                SequenceMatch::None => {
                    if !self.in_lookback_mode {
                        let buf = self.lookback_input_buffer.clone();
                        self.write_pty(&buf)?;
                    }
                    self.lookback_input_buffer.clear();
                }
            }
        }
        Ok(())
    }

    fn check_sequence_match(
        &self,
        byte: u8,
        buffer: &mut Vec<u8>,
        sequence: &[u8],
    ) -> SequenceMatch {
        buffer.push(byte);
        if buffer.len() > sequence.len() {
            let excess = buffer.len() - sequence.len();
            buffer.drain(..excess);
        }
        if buffer.as_slice() == sequence {
            SequenceMatch::Complete
        } else if sequence.starts_with(buffer) {
            SequenceMatch::Partial
        } else {
            SequenceMatch::None
        }
    }

    // -----------------------------------------------------------------------
    // Lookback mode
    // -----------------------------------------------------------------------

    fn enter_lookback_mode(&mut self) -> Result<()> {
        debug!(
            "enter_lookback_mode: history_bytes={} lines={}",
            self.history.total_bytes(),
            self.history.line_count()
        );
        self.in_lookback_mode = true;
        self.lookback_cache.clear();
        self.vt_render_pending = false;

        self.output_buffer.clear();
        self.history.append_all(&mut self.output_buffer);
        debug!(
            "enter_lookback_mode: output_buffer_len={}",
            self.output_buffer.len()
        );

        Self::write_stdout(CLEAR_SCREEN)?;
        Self::write_stdout(CURSOR_HOME)?;
        Self::write_stdout(&self.output_buffer)?;

        let exit_msg = format!(
            "\r\n\x1b[7m--- LOOKBACK MODE: press {} or Ctrl+C to exit ---\x1b[0m\r\n",
            self.config.lookback_key
        );
        Self::write_stdout(exit_msg.as_bytes())?;

        Ok(())
    }

    fn exit_lookback_mode(&mut self) -> Result<()> {
        debug!(
            "exit_lookback_mode: cached_len={}",
            self.lookback_cache.len()
        );
        self.in_lookback_mode = false;

        let cached = std::mem::take(&mut self.lookback_cache);
        if !cached.is_empty() {
            debug!(
                "exit_lookback_mode: processing {} cached bytes",
                cached.len()
            );
            self.process_output(&cached)?;
        }

        self.in_sync_block = false;
        self.sync_buffer.clear();

        self.check_resize()?;

        debug!("exit_lookback_mode: rendering VT screen");
        self.vt_prev_screen = None;
        self.render_vt_screen()?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Window resize (polled from main loop timeout)
    // -----------------------------------------------------------------------

    fn check_resize(&mut self) -> Result<()> {
        let (cols, rows) = get_terminal_size();
        if cols != self.last_cols || rows != self.last_rows {
            debug!(
                "check_resize: {}x{} -> {}x{}",
                self.last_cols, self.last_rows, cols, rows
            );
            self.last_cols = cols;
            self.last_rows = rows;
            self.vt_parser.screen_mut().set_size(rows, cols);
            self.vt_prev_screen = None;
            let size = COORD {
                X: cols as i16,
                Y: rows as i16,
            };
            unsafe {
                ResizePseudoConsole(self.hpc, size);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Child process
    // -----------------------------------------------------------------------

    fn wait_child(&mut self) -> Result<i32> {
        unsafe {
            WaitForSingleObject(self.process_handle, INFINITE_WAIT);
            let mut exit_code: DWORD = 0;
            if GetExitCodeProcess(self.process_handle, &mut exit_code) != 0 {
                Ok(exit_code as i32)
            } else {
                Ok(1)
            }
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // Disable bracketed paste
        let _ = Self::write_stdout(BRACKETED_PASTE_DISABLE);

        // Restore console modes
        unsafe {
            let stdin = GetStdHandle(STD_INPUT_HANDLE);
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            SetConsoleMode(stdin, self.original_stdin_mode);
            SetConsoleMode(stdout, self.original_stdout_mode);
        }

        // Close ConPTY and process handles
        unsafe {
            ClosePseudoConsole(self.hpc);
            CloseHandle(self.pty_input.0);
            // pty_output.0 may already be closed by reader thread exit,
            // but CloseHandle on an already-closed handle is harmless on Windows
            CloseHandle(self.pty_output.0);
            CloseHandle(self.process_handle);
            CloseHandle(self.thread_handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn split_trailing_marker_prefix<'a>(data: &'a [u8], marker: &[u8]) -> (&'a [u8], &'a [u8]) {
    let max_prefix = marker.len().saturating_sub(1).min(data.len());
    for prefix_len in (1..=max_prefix).rev() {
        if data.ends_with(&marker[..prefix_len]) {
            let split = data.len() - prefix_len;
            return (&data[..split], &data[split..]);
        }
    }
    (data, &[])
}
