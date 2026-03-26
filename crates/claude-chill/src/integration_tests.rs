#[cfg(test)]
mod tests {
    use crate::proxy::{Proxy, ProxyConfig};
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use nix::pty::openpty;
    use nix::sys::termios::{self, SetArg};
    use nix::unistd::read;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::time::Duration;

    fn pty_mock_binary() -> String {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        format!("{}/../../target/debug/pty-mock", manifest_dir)
    }

    fn spawn_command(cmd: &str, args: &[&str]) -> (OwnedFd, Child) {
        let pty = openpty(None, None).expect("openpty failed");

        let slave_fd = pty.slave.as_raw_fd();
        let mut termios_settings = termios::tcgetattr(&pty.slave).expect("tcgetattr failed");
        termios::cfmakeraw(&mut termios_settings);
        termios::tcsetattr(&pty.slave, SetArg::TCSANOW, &termios_settings)
            .expect("tcsetattr failed");

        let child = unsafe {
            Command::new(cmd)
                .args(args)
                .stdin(pty.slave.try_clone().expect("clone slave"))
                .stdout(pty.slave.try_clone().expect("clone slave"))
                .stderr(pty.slave.try_clone().expect("clone slave"))
                .pre_exec(move || {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .expect("failed to spawn command")
        };

        drop(pty.slave);

        let flags = fcntl(&pty.master, FcntlArg::F_GETFL).expect("F_GETFL");
        let flags = OFlag::from_bits_truncate(flags);
        fcntl(&pty.master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("F_SETFL");

        (pty.master, child)
    }

    fn spawn_mock(subcommand: &str) -> (OwnedFd, Child) {
        let pty = openpty(None, None).expect("openpty failed");

        // Set slave to raw mode so escape sequences pass through unmodified
        let slave_fd = pty.slave.as_raw_fd();
        let mut termios_settings = termios::tcgetattr(&pty.slave).expect("tcgetattr failed");
        termios::cfmakeraw(&mut termios_settings);
        termios::tcsetattr(&pty.slave, SetArg::TCSANOW, &termios_settings)
            .expect("tcsetattr failed");

        let child = unsafe {
            Command::new(pty_mock_binary())
                .arg(subcommand)
                .stdin(pty.slave.try_clone().expect("clone slave"))
                .stdout(pty.slave.try_clone().expect("clone slave"))
                .stderr(pty.slave.try_clone().expect("clone slave"))
                .pre_exec(move || {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .expect("failed to spawn pty-mock")
        };

        drop(pty.slave);

        // Set master to non-blocking
        let flags = fcntl(&pty.master, FcntlArg::F_GETFL).expect("F_GETFL");
        let flags = OFlag::from_bits_truncate(flags);
        fcntl(&pty.master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("F_SETFL");

        (pty.master, child)
    }

    fn read_pty_output(master: &OwnedFd, timeout_ms: u16) -> Vec<u8> {
        let mut result = Vec::new();
        let mut buf = [0u8; 65536];
        let mut poll_fds = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];

        loop {
            match poll(&mut poll_fds, PollTimeout::from(timeout_ms)) {
                Ok(0) => break,
                Ok(_) => match read(master.as_fd(), &mut buf) {
                    Ok(0) => break,
                    Ok(n) => result.extend_from_slice(&buf[..n]),
                    Err(nix::errno::Errno::EAGAIN) => break,
                    Err(_) => break,
                },
                Err(_) => break,
            }
        }
        result
    }

    fn make_proxy(master: OwnedFd, child: Child) -> Proxy {
        let config = ProxyConfig::default();
        Proxy::new_for_test(master, child, config, 24, 80)
    }

    fn dev_null() -> OwnedFd {
        use std::fs::OpenOptions;
        let f = OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("open /dev/null");
        OwnedFd::from(f)
    }

    fn capture_pipe() -> (OwnedFd, OwnedFd) {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe failed");
        // Set read end to non-blocking
        let flags = fcntl(&read_fd, FcntlArg::F_GETFL).expect("F_GETFL");
        let flags = OFlag::from_bits_truncate(flags);
        fcntl(&read_fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("F_SETFL");
        (read_fd, write_fd)
    }

    fn read_all_from_fd(fd: &OwnedFd) -> Vec<u8> {
        let mut result = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            match read(fd.as_fd(), &mut buf) {
                Ok(0) => break,
                Ok(n) => result.extend_from_slice(&buf[..n]),
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(_) => break,
            }
        }
        result
    }

    fn dup_master(proxy: &Proxy) -> OwnedFd {
        let raw = proxy.pty_master_fd_raw();
        unsafe { OwnedFd::from_raw_fd(libc::dup(raw)) }
    }

    #[test]
    fn test_echo_baseline() {
        let (master, child) = spawn_mock("echo");
        let mut proxy = make_proxy(master, child);
        let sink = dev_null();

        proxy
            .process_input(b"hello world\r\n", &sink)
            .expect("process_input");
        proxy.flush_drain_buffer(&sink).expect("flush drain");

        std::thread::sleep(Duration::from_millis(50));

        let master_dup = dup_master(&proxy);
        let output = read_pty_output(&master_dup, 100);
        assert!(!output.is_empty(), "expected output from echo child");
        proxy
            .process_output(&output, &sink)
            .expect("process_output");

        let mut history = Vec::new();
        proxy.history().append_all(&mut history);
        let history_str = String::from_utf8_lossy(&history);
        assert!(
            history_str.contains("hello world"),
            "history should contain echoed text, got: {:?}",
            history_str
        );
    }

    #[test]
    fn test_alt_screen_tracking() {
        let (master, child) = spawn_mock("alt-screen");
        let mut proxy = make_proxy(master, child);
        let sink = dev_null();

        std::thread::sleep(Duration::from_millis(50));

        let master_dup = dup_master(&proxy);
        let output = read_pty_output(&master_dup, 100);
        if !output.is_empty() {
            proxy
                .process_output(&output, &sink)
                .expect("process_output");
        }

        assert!(
            proxy.is_in_alternate_screen(),
            "proxy should be in alt screen"
        );

        proxy.process_input(b"x", &sink).expect("process_input");
        proxy.flush_drain_buffer(&sink).expect("flush drain");

        std::thread::sleep(Duration::from_millis(50));

        let master_dup = dup_master(&proxy);
        let output = read_pty_output(&master_dup, 100);
        if !output.is_empty() {
            proxy
                .process_output(&output, &sink)
                .expect("process_output");
        }

        assert!(
            !proxy.is_in_alternate_screen(),
            "proxy should have exited alt screen"
        );
    }

    #[test]
    fn test_bracketed_paste_flow() {
        let (master, child) = spawn_mock("paste-echo");
        let mut proxy = make_proxy(master, child);
        let sink = dev_null();

        std::thread::sleep(Duration::from_millis(50));
        let master_dup = dup_master(&proxy);
        let output = read_pty_output(&master_dup, 100);
        if !output.is_empty() {
            proxy
                .process_output(&output, &sink)
                .expect("process_output");
        }

        let mut paste = Vec::new();
        paste.extend_from_slice(b"\x1b[200~");
        paste.extend_from_slice(b"pasted content\r\n");
        paste.extend_from_slice(b"\x1b[201~");

        proxy.process_input(&paste, &sink).expect("process_input");
        proxy.flush_drain_buffer(&sink).expect("flush drain");

        assert!(
            !proxy.is_in_bracketed_paste(),
            "paste mode should be off after complete paste"
        );
    }

    #[test]
    fn test_split_paste_end_marker() {
        let (master, child) = spawn_mock("paste-echo");
        let mut proxy = make_proxy(master, child);
        let sink = dev_null();

        std::thread::sleep(Duration::from_millis(50));
        let master_dup = dup_master(&proxy);
        let output = read_pty_output(&master_dup, 100);
        if !output.is_empty() {
            proxy
                .process_output(&output, &sink)
                .expect("process_output");
        }

        let mut chunk1 = Vec::new();
        chunk1.extend_from_slice(b"\x1b[200~");
        chunk1.extend_from_slice(b"split test\r\n");
        chunk1.extend_from_slice(b"\x1b[20"); // partial end marker

        proxy
            .process_input(&chunk1, &sink)
            .expect("process_input chunk1");
        proxy.flush_drain_buffer(&sink).expect("flush drain");

        assert!(
            proxy.is_in_bracketed_paste(),
            "should still be in paste mode after partial end marker"
        );

        proxy
            .process_input(b"1~", &sink)
            .expect("process_input chunk2");
        proxy.flush_drain_buffer(&sink).expect("flush drain");

        assert!(
            !proxy.is_in_bracketed_paste(),
            "paste mode should be off after completing split end marker"
        );
    }

    #[test]
    fn test_alt_screen_exit_restores_main_content() {
        let (master, child) = spawn_mock("echo");
        let mut proxy = make_proxy(master, child);
        let sink = dev_null();

        // Simulate child writing main screen content
        let main_content = b"Claude Code v2.1.84\r\nOpus 4.6\r\n~/public/claude-chill\r\n";
        proxy
            .process_output(main_content, &sink)
            .expect("process_output main content");

        let screen_before = proxy.vt_screen_text();
        assert!(
            screen_before.contains("Claude Code v2.1.84"),
            "main content should be on screen before alt screen, got: {:?}",
            screen_before
        );

        // Child enters alt screen
        proxy
            .process_output(b"\x1b[?1049h", &sink)
            .expect("process_output alt enter");
        assert!(proxy.is_in_alternate_screen());

        // Child writes alt screen content (e.g. vim, less)
        proxy
            .process_output(b"alt screen editor content\r\nline 2\r\n", &sink)
            .expect("process_output alt content");

        // Child exits alt screen
        proxy
            .process_output(b"\x1b[?1049l", &sink)
            .expect("process_output alt exit");
        assert!(!proxy.is_in_alternate_screen());

        // The VT parser should have restored the main screen content
        let screen_after = proxy.vt_screen_text();
        assert!(
            screen_after.contains("Claude Code v2.1.84"),
            "main content should be restored after alt screen exit, got: {:?}",
            screen_after
        );
    }

    #[test]
    fn test_alt_screen_exit_restores_after_unrendered_sync_block() {
        // Reproduces the bug from the log: a sync block updates the VT parser
        // but render_vt_screen never fires before alt-screen-enter arrives
        // in the next chunk. After alt-screen-exit, the full VT render
        // should produce output that makes the real terminal show the correct
        // main screen content.
        //
        // We capture what the proxy writes to "stdout" and feed it through
        // a second VT parser to simulate what the real terminal would display.
        let (master, child) = spawn_mock("echo");
        let mut proxy = make_proxy(master, child);
        let (capture_read, capture_write) = capture_pipe();

        // Initial content rendered to screen via sync block
        let mut initial_sync = Vec::new();
        initial_sync.extend_from_slice(b"\x1b[?2026h");
        initial_sync.extend_from_slice(b"\x1b[H");
        initial_sync.extend_from_slice(b"Claude Code v2.1.84\r\n");
        initial_sync.extend_from_slice(b"Opus 4.6\r\n");
        initial_sync.extend_from_slice(b"~/public/claude-chill\r\n");
        initial_sync.extend_from_slice(b"\x1b[?2026l");

        proxy
            .process_output(&initial_sync, &capture_write)
            .expect("initial sync block");

        // Drain what was written so far
        let _ = read_all_from_fd(&capture_read);

        // Second sync block updates the VT parser but render hasn't fired
        let mut update_sync = Vec::new();
        update_sync.extend_from_slice(b"\x1b[?2026h");
        update_sync.extend_from_slice(b"\x1b[4;1H");
        update_sync.extend_from_slice(b"updated status line\r\n");
        update_sync.extend_from_slice(b"\x1b[?2026l");

        proxy
            .process_output(&update_sync, &capture_write)
            .expect("update sync block");

        // Capture what the proxy rendered for the update
        let rendered_update = read_all_from_fd(&capture_read);

        // Build a "real terminal" VT parser and feed it what the proxy rendered
        let mut real_terminal = vt100::Parser::new(24, 80, 0);
        // Feed the initial render output
        real_terminal.process(&initial_sync);
        // Feed the update render (this is what the proxy wrote to stdout)
        real_terminal.process(&rendered_update);

        let real_screen_before_alt = real_terminal.screen().contents();

        // Alt screen enter in the NEXT chunk (before render timer fires)
        proxy
            .process_output(b"\x1b[?1049h", &capture_write)
            .expect("alt screen enter");

        let alt_enter_output = read_all_from_fd(&capture_read);
        real_terminal.process(&alt_enter_output);

        // Alt screen content
        proxy
            .process_output(b"editor content here\r\n", &capture_write)
            .expect("alt screen content");
        let alt_content_output = read_all_from_fd(&capture_read);
        real_terminal.process(&alt_content_output);

        // Alt screen exit — this triggers render_vt_screen(diff=false)
        proxy
            .process_output(b"\x1b[?1049l", &capture_write)
            .expect("alt screen exit");
        let alt_exit_output = read_all_from_fd(&capture_read);
        real_terminal.process(&alt_exit_output);

        let real_screen_after = real_terminal.screen().contents();

        // The real terminal should show the same content it had before
        // alt screen was entered
        assert!(
            real_screen_after.contains("Claude Code v2.1.84"),
            "real terminal should show main content after alt screen exit, got: {:?}",
            real_screen_after
        );
        assert!(
            real_screen_after.contains("Opus 4.6"),
            "real terminal should show Opus line after alt screen exit, got: {:?}",
            real_screen_after
        );
    }

    fn pump_proxy(proxy: &mut Proxy, sink: &OwnedFd, timeout_ms: u16) {
        let master_dup = dup_master(proxy);
        let output = read_pty_output(&master_dup, timeout_ms);
        if !output.is_empty() {
            proxy.process_output(&output, sink).expect("process_output");
        }
    }

    fn send_input(proxy: &mut Proxy, sink: &OwnedFd, data: &[u8]) {
        proxy.process_input(data, sink).expect("process_input");
        proxy.flush_drain_buffer(sink).expect("flush drain");
    }

    fn wait_for_screen(proxy: &mut Proxy, sink: &OwnedFd, text: &str, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            pump_proxy(proxy, sink, 100);
            if proxy.vt_screen_text().contains(text) {
                return true;
            }
        }
        false
    }

    #[test]
    #[ignore] // requires claude CLI to be installed and authenticated
    fn test_claude_alt_screen_restore() {
        let (master, child) = spawn_command("claude", &[]);
        let mut proxy = make_proxy(master, child);
        let (capture_read, capture_write) = capture_pipe();

        // Wait for Claude Code to start up and show its UI
        assert!(
            wait_for_screen(
                &mut proxy,
                &capture_write,
                "Claude Code",
                Duration::from_secs(10)
            ),
            "Claude Code did not start, screen: {:?}",
            proxy.vt_screen_text()
        );

        let screen_before = proxy.vt_screen_text();
        eprintln!("Screen before alt screen:\n{}", screen_before);

        // Build a real terminal model from everything written so far
        let mut real_terminal = vt100::Parser::new(24, 80, 0);
        let rendered_so_far = read_all_from_fd(&capture_read);
        real_terminal.process(&rendered_so_far);

        // Send Ctrl-G to open the editor (enters alt screen)
        send_input(&mut proxy, &capture_write, &[0x07]); // Ctrl-G

        // Wait for alt screen to be entered
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            pump_proxy(&mut proxy, &capture_write, 100);
            if proxy.is_in_alternate_screen() {
                break;
            }
        }
        assert!(
            proxy.is_in_alternate_screen(),
            "should have entered alt screen after Ctrl-G"
        );

        // Feed real terminal what the proxy wrote during alt screen enter
        let alt_enter_output = read_all_from_fd(&capture_read);
        real_terminal.process(&alt_enter_output);

        // Brief pause for the editor to fully render
        std::thread::sleep(Duration::from_millis(500));
        pump_proxy(&mut proxy, &capture_write, 200);
        let mid_output = read_all_from_fd(&capture_read);
        real_terminal.process(&mid_output);

        // Send :wq<Enter> to close the editor (exits alt screen)
        send_input(&mut proxy, &capture_write, b"\x1b");
        std::thread::sleep(Duration::from_millis(100));
        send_input(&mut proxy, &capture_write, b":wq\r");

        // Wait for alt screen to be exited
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            pump_proxy(&mut proxy, &capture_write, 100);
            if !proxy.is_in_alternate_screen() {
                break;
            }
        }

        // Drain remaining output after alt screen exit
        std::thread::sleep(Duration::from_millis(500));
        pump_proxy(&mut proxy, &capture_write, 200);

        let alt_exit_output = read_all_from_fd(&capture_read);
        real_terminal.process(&alt_exit_output);

        let real_screen_after = real_terminal.screen().contents();
        let vt_screen_after = proxy.vt_screen_text();

        eprintln!("VT parser screen after alt exit:\n{}", vt_screen_after);
        eprintln!(
            "Real terminal screen after alt exit:\n{}",
            real_screen_after
        );

        // The real terminal should show Claude Code's main UI
        assert!(
            real_screen_after.contains("Claude Code"),
            "real terminal should show Claude Code UI after alt screen exit, got: {:?}",
            real_screen_after
        );

        // Send /exit to quit
        send_input(&mut proxy, &capture_write, b"/exit\r");
        std::thread::sleep(Duration::from_secs(1));
    }
}
