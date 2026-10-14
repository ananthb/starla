//! The controller's telnet session, scripted over an in-memory stream: the
//! exact bytes the probe writes back, against the reference's telnetd
//! (networking/telnetd.c, ATLAS states) and the 2026-10-13 twin run, where
//! the reference answered each request with one "OK\r\n\r\n".

use super::*;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

const SID: &str = "5345a14cdcdc8505d6f95eaabcc55421b2b01bfbe239557186587c895b5508d4";

/// The banner, byte for byte as the twin captured it from both probes.
fn banner() -> Vec<u8> {
    let host = hostname::get().unwrap().to_string_lossy().to_string();
    let mut banner = b"\xff\xfd\x01\xff\xfd\x1f\xff\xfb\x01\xff\xfb\x03".to_vec();
    banner.extend_from_slice(
        format!("Atlas probe, see http://atlas.ripe.net/\r\n\r\nProbe 1018212 ({host}) login: ")
            .as_bytes(),
    );
    banner
}

/// Play `input` as the controller, then close: what the probe wrote after
/// its banner, and the commands it handed on.
async fn session_with(session_id: Option<&str>, input: &[u8]) -> (String, Vec<TelnetCommand>) {
    let (controller, probe) = tokio::io::duplex(1 << 16);
    let (tx, mut rx) = mpsc::channel(1024);
    let sid = Arc::new(tokio::sync::RwLock::new(session_id.map(str::to_string)));
    let handler = tokio::spawn(handle_connection(probe, Some(tx), 1018212, sid));

    let (mut from_probe, mut to_probe) = tokio::io::split(controller);
    let write = async {
        // The probe may close first (a login name it does not take)
        let _ = to_probe.write_all(input).await;
        let _ = to_probe.shutdown().await;
    };
    let read = async {
        let mut out = Vec::new();
        from_probe.read_to_end(&mut out).await.unwrap();
        out
    };
    let ((), out) = tokio::join!(write, read);
    handler.await.unwrap().unwrap();

    let banner = banner();
    assert!(
        out.starts_with(&banner),
        "banner: {:?}",
        String::from_utf8_lossy(&out)
    );
    let mut commands = Vec::new();
    while let Some(command) = rx.recv().await {
        commands.push(command);
    }
    (
        String::from_utf8(out[banner.len()..].to_vec()).unwrap(),
        commands,
    )
}

async fn session(input: &str) -> (String, Vec<TelnetCommand>) {
    session_with(Some(SID), input.as_bytes()).await
}

fn login() -> String {
    format!("C_TO_P_TEST_V1\n{SID}\n")
}

/// What `lines` are once parsed, to compare with the commands a session
/// handed on. The handler owns which lines it hands on; how a line parses
/// is parse_command's, so the tests compare with it rather than pin it.
fn parsed<S: AsRef<str>>(lines: &[S]) -> String {
    let commands: Vec<_> = lines.iter().map(|l| parse_command(l.as_ref())).collect();
    for command in &commands {
        assert!(!matches!(command, TelnetCommand::Unknown(_)), "a known command");
    }
    format!("{commands:?}")
}

fn handed_on(commands: &[TelnetCommand]) -> String {
    format!("{commands:?}")
}

/// A CRONLINE as a crontab holds it: what add_to_crontab writes (the line
/// after "CRONLINE "), and how parse_command reads it.
fn crontab_line(line: &str) -> CrontabLine {
    let command = parse_command(line);
    assert!(!matches!(command, TelnetCommand::Unknown(_)), "a known command");
    CrontabLine {
        line: line.strip_prefix("CRONLINE ").unwrap().to_string(),
        command,
    }
}

/// The crontab of `/home/atlas/crons/<dir>` with `lines`, as it is handed
/// on once it has ended: its directory rebased onto the spool directory.
fn crontab(dir: &str, lines: &[&str]) -> TelnetCommand {
    TelnetCommand::Crontab(CrontabSpec {
        dir: format!("/var/spool/ripe-atlas/crons/{dir}"),
        lines: lines.iter().map(|l| crontab_line(l)).collect(),
    })
}

/// The crontabs of a request's lines (CRONTAB lines, each followed by its
/// CRONLINEs), each whole.
fn crontabs<S: AsRef<str>>(lines: &[S]) -> Vec<TelnetCommand> {
    let mut crontabs = Vec::new();
    for line in lines {
        let line = line.as_ref();
        if let Some(dir) = line.strip_prefix("CRONTAB /home/atlas/crons/") {
            crontabs.push(crontab(dir, &[]));
        } else {
            let Some(TelnetCommand::Crontab(spec)) = crontabs.last_mut() else {
                panic!("{line}");
            };
            spec.lines.push(crontab_line(line));
        }
    }
    crontabs
}

/// The request shape of the twin's first batch (scenario
/// enodia-2026-10-13T0131, from the enodia journal): three CRONTABs and
/// their CRONLINEs, 201 lines.
fn crontab_request() -> Vec<String> {
    let mut lines = Vec::new();
    for (dir, n) in [("main", 2), ("2", 99), ("7", 97)] {
        lines.push(format!("CRONTAB /home/atlas/crons/{dir}"));
        for i in 0..n {
            lines.push(format!(
                "CRONLINE 240 {i} 1794447121 UNIFORM 3 evtdig -6 -h -A \"{}\" -O /home/atlas/data/new/7 2001:7fd::1",
                11000 + i
            ));
        }
    }
    lines[2] = "CRONLINE 240 1780657136 1791883921 UNIFORM 14 evping -6 -c 3 -s 64 -i 1000 -A \"177421599\" -O /home/atlas/data/new/7 www.office.com".to_string();
    assert_eq!(lines.len(), 201);
    lines
}

#[tokio::test]
async fn login_and_password_get_no_ok() {
    // telnetd.c:1080-1086 prompts for the password; :1108-1127 takes it and
    // says nothing. Starla sent "OK\r\n\r\n" here.
    let (out, commands) = session(&login()).await;
    assert_eq!(out, "\r\nPassword: ");
    assert!(commands.is_empty());
}

#[tokio::test]
async fn a_crontab_request_gets_one_ok_on_its_empty_line() {
    // Twin batch 0: the reference wrote exactly "OK\r\n\r\n" for 201 lines and
    // the empty line; Starla wrote one OK after the password and one per line.
    let lines = crontab_request();
    let request = format!("{}{}\n\n", login(), lines.join("\n"));
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: OK\r\n\r\n");

    // Each crontab is handed on whole once a line ends it: main and 2 by
    // the next CRONTAB, 7 by the empty line (end_crontab,
    // telnetd.c:1535-1600)
    assert_eq!(commands.len(), 3);
    assert_eq!(handed_on(&commands), handed_on(&crontabs(&lines)));

    // No empty line, no OK: the request has not ended (Starla used to skip
    // the empty line and answer every other one). Crontab 7 is still open
    // when the session ends, and the reference closes it and never installs
    // it (kill_session, :1302-1311); Starla handed on its lines as they came,
    // and acted on a crontab the controller never finished.
    let request = format!("{}{}\n", login(), lines.join("\n"));
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: ");
    assert_eq!(handed_on(&commands), handed_on(&crontabs(&lines)[..2]));
}

#[tokio::test]
async fn a_crontab_ends_at_the_first_line_that_is_not_a_cronline() {
    // DO_CRONTAB ends the crontab at any other line, which is then the next
    // command (telnetd.c:1241-1256): the next CRONTAB, a ONEOFF, the empty
    // line, anything. The spool directory's own spelling is the same
    // directory (rebased_validated_filename).
    let a = "CRONLINE 60 12 2000000000 UNIFORM 20 httppost --post-dir /home/atlas/data/out http://127.0.0.1:8080/?PROBE_ID=1018212";
    let b = "CRONLINE 600 1791854199 1791854229 UNIFORM 3 evtdig -4 --resolv --qbuf --type 1 --class 1 --query jd.com. -A \"30002\" -O /home/atlas/data/new/7";
    let c = "CRONLINE 60 1 2000000000 UNIFORM 14 condmv -A \"RESULT 9901 ongoing\" /home/atlas/data/new/2 /home/atlas/data/out/2";
    let oneoff = "ONEOFF /home/atlas/crons/oneoff evping -A \"1\" 192.0.2.1";
    let request = format!(
        "{}CRONTAB /home/atlas/crons/main\n{a}\nCRONTAB /home/atlas/crons/7\n{b}\n{oneoff}\n\
         CRONTAB /var/spool/ripe-atlas/crons/2\n{c}\n\n",
        login()
    );
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: OK\r\n\r\n");
    let mut wanted = vec![crontab("main", &[a]), crontab("7", &[b])];
    wanted.push(parse_command(oneoff));
    wanted.push(crontab("2", &[c]));
    assert_eq!(handed_on(&commands), handed_on(&wanted));

    // A crontab with no CRONLINE is an empty file: it is handed on, and
    // empties its directory's crontab. A bad command ends one too.
    let request = format!(
        "{}CRONTAB /home/atlas/crons/7\nCRONTAB /home/atlas/crons/2\nFOO\n\n",
        login()
    );
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: BAD_COMMAND\r\n\r\nOK\r\n\r\n");
    assert_eq!(
        handed_on(&commands),
        handed_on(&[crontab("7", &[]), crontab("2", &[])])
    );
}

#[tokio::test]
async fn a_crontab_its_session_leaves_open_is_never_handed_on() {
    // The connection goes mid-crontab: kill_session closes root.new, which
    // is never renamed into place (telnetd.c:1302-1311)
    let b = "CRONLINE 600 1791854199 1791854229 UNIFORM 3 evtdig -4 --resolv --qbuf --type 1 --class 1 --query jd.com. -A \"30002\" -O /home/atlas/data/new/7";
    let request = format!("{}CRONTAB /home/atlas/crons/7\n{b}\n", login());
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: ");
    assert!(commands.is_empty(), "commands were sent");

    // A newer session takes over mid-crontab: the old one's is closed and
    // never installed (:1112-1122), and the newer session's crontab is
    // the one handed on.
    let probe = Arc::new(tokio::sync::RwLock::new(Some(SID.to_string())));
    let mut first = open(&probe);
    authenticate(&mut first).await;
    first
        .controller
        .write_all(format!("CRONTAB /home/atlas/crons/7\n{b}\n").as_bytes())
        .await
        .unwrap();
    // Wait until the first session has taken the CRONLINE in: it logs each
    // line before it takes it, and takes a CRONLINE without an await
    Journal::open();
    let taken = format!("Telnet state Authenticated, received: '{b}'");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !Journal::has(&taken) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first session takes its CRONLINE");
    Journal::close();
    let mut second = open(&probe);
    authenticate(&mut second).await;
    first.handler.await.unwrap().unwrap();
    assert!(first.commands.recv().await.is_none(), "nothing handed on");

    let c = "CRONLINE 240 17 2000000000 UNIFORM 3 evping -6 -c 3 -A \"1002\" 2001:7fd::1";
    second
        .controller
        .write_all(format!("CRONTAB /home/atlas/crons/7\n{c}\n\n").as_bytes())
        .await
        .unwrap();
    let command = second.commands.recv().await.unwrap();
    assert_eq!(handed_on(&[command]), handed_on(&[crontab("7", &[c])]));
}

#[tokio::test]
async fn a_crontab_is_read_no_further_than_its_cron_daemon_reads() {
    // eperd and perd read at most 65534 lines of a crontab (maxLines,
    // eperd.c:542-549, perd.c:458-474): the lines after those are not taken
    let line = |i: usize| format!("CRONLINE 60 {i} 2000000000 NONE 0 evping -A \"{i}\" 192.0.2.1");
    let mut request = format!("{}CRONTAB /home/atlas/crons/7\n", login());
    for i in 0..CRONTAB_MAX_LINES + 2 {
        request += &line(i);
        request.push('\n');
    }
    request.push('\n');
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: OK\r\n\r\n");
    let [TelnetCommand::Crontab(crontab)] = &commands[..] else {
        panic!("{} commands", commands.len());
    };
    assert_eq!(crontab.lines.len(), CRONTAB_MAX_LINES);
    let last = line(CRONTAB_MAX_LINES - 1);
    assert_eq!(
        crontab.lines.last().unwrap().line,
        last.strip_prefix("CRONLINE ").unwrap()
    );
}

#[tokio::test]
async fn a_oneoff_request_gets_one_ok() {
    // Twin batch 2: the reference "OK\r\n\r\n", Starla "OK\r\n\r\nOK\r\n\r\n".
    let oneoff = "ONEOFF /home/atlas/crons/oneoff evping -6 -c 3 -s 48 -i 1000 -A \"222024177\" 2001:db8:7769:6e00::20";
    let (out, commands) = session(&format!("{}{oneoff}\n\n", login())).await;
    assert_eq!(out, "\r\nPassword: OK\r\n\r\n");
    assert_eq!(handed_on(&commands), parsed(&[oneoff]));
}

#[tokio::test]
async fn crlf_line_ends_make_no_extra_empty_line() {
    // "\r\n" is one line end. Split at both bytes, every line would be
    // followed by an empty one: the request would end (OK) on the CRONTAB's,
    // and every CRONLINE after it would be a BAD_COMMAND.
    let lines = crontab_request();
    let request = format!("C_TO_P_TEST_V1\r\n{SID}\r\n{}\r\n\r\n", lines.join("\r\n"));
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: OK\r\n\r\n");
    assert_eq!(handed_on(&commands), handed_on(&crontabs(&lines)));

    // As getline_2pty, a bare '\r' ends a line too, and a telnet client's
    // CR NUL is one line end.
    let oneoff = "ONEOFF /home/atlas/crons/oneoff evping -A \"1\" 192.0.2.1";
    let request = format!("C_TO_P_TEST_V1\r{SID}\r\0{oneoff}\r\r");
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: OK\r\n\r\n");
    assert_eq!(handed_on(&commands), parsed(&[oneoff]));
}

#[test]
fn line_ends_split_across_reads() {
    let mut lines = LineSplitter::default();
    assert_eq!(
        lines.feed(b"CRONTAB /home/atlas/crons/main\r"),
        ["CRONTAB /home/atlas/crons/main"]
    );
    // The '\n' of that "\r\n" comes in the next read: no empty line
    assert!(lines.feed(b"\n").is_empty());
    assert_eq!(lines.feed(b"CRONLINE 1"), Vec::<String>::new());
    assert_eq!(lines.pending(), 10);
    assert_eq!(lines.feed(b" 2\r\n\r\n"), ["CRONLINE 1 2", ""]);
    assert_eq!(lines.feed(b"a\r\0b\n\nc\r\r"), ["a", "b", "", "c", ""]);
    // A line with a NUL inside is skipped (telnetd.c:1432-1445)
    assert_eq!(lines.feed(b"x\0y\nz\n"), ["z"]);
    assert_eq!(lines.pending(), 0);
}

#[tokio::test]
async fn an_unknown_command_gets_bad_command() {
    // telnetd.c:1223-1227. Starla answered "ERROR: Unknown command: …".
    // A CRONLINE outside a CRONTAB is one too, and is not taken.
    let request = format!(
        "{}FOO bar\nCRONLINE 240 1 1794447121 UNIFORM 3 evping -A \"1\" 192.0.2.1\ncrontab /home/atlas/crons/main\n\n",
        login()
    );
    let (out, commands) = session(&request).await;
    assert_eq!(
        out,
        "\r\nPassword: BAD_COMMAND\r\n\r\nBAD_COMMAND\r\n\r\nBAD_COMMAND\r\n\r\nOK\r\n\r\n"
    );
    assert!(commands.is_empty());
}

#[tokio::test]
async fn after_the_empty_line_every_line_is_a_bad_command() {
    // EOM_SEEN eats all input and answers BAD_COMMAND (telnetd.c:1265-1278),
    // a second empty line and a second request included; none is taken.
    let first = "ONEOFF /home/atlas/crons/oneoff evping -A \"1\" 192.0.2.1";
    let second = "ONEOFF /home/atlas/crons/oneoff evping -A \"2\" 192.0.2.1";
    let request = format!("{}{first}\n\n{second}\n\n", login());
    let (out, commands) = session(&request).await;
    assert_eq!(
        out,
        "\r\nPassword: OK\r\n\r\nBAD_COMMAND\r\n\r\nBAD_COMMAND\r\n\r\n"
    );
    assert_eq!(handed_on(&commands), parsed(&[first]));
}

#[tokio::test]
async fn a_bad_password_gets_bad_password_and_another_try() {
    // telnetd.c:1129-1136: BAD_PASSWORD, the state stays GET_PASSWORD, and the
    // connection stays open. Starla closed it.
    let request = format!(
        "C_TO_P_TEST_V1\nwrong\n{SID}\nONEOFF /home/atlas/crons/oneoff evping -A \"1\" 192.0.2.1\n\n"
    );
    let (out, commands) = session(&request).await;
    assert_eq!(out, "\r\nPassword: BAD_PASSWORD\r\n\r\nOK\r\n\r\n");
    assert_eq!(commands.len(), 1);

    // No session ID yet: every password is bad, an empty one too, and no line
    // gets past the password.
    let request = format!(
        "C_TO_P_TEST_V1\n{SID}\n\nONEOFF /home/atlas/crons/oneoff evping -A \"1\" 192.0.2.1\n"
    );
    let (out, commands) = session_with(None, request.as_bytes()).await;
    assert_eq!(
        out,
        "\r\nPassword: BAD_PASSWORD\r\n\r\nBAD_PASSWORD\r\n\r\nBAD_PASSWORD\r\n\r\n"
    );
    assert!(commands.is_empty());
}

#[tokio::test]
async fn another_login_name_is_echoed_and_the_connection_closed() {
    // The reference echoes it and starts /bin/login (telnetd.c:1087-1100);
    // Starla echoes it and has no login to start. Nothing after it is read.
    let request =
        format!("root\n{SID}\nONEOFF /home/atlas/crons/oneoff evping -A \"1\" 192.0.2.1\n\n");
    let (out, commands) = session(&request).await;
    assert_eq!(out, "root");
    assert!(commands.is_empty());
}

#[tokio::test]
async fn bad_paths_and_long_names() {
    // start_crontab and do_oneoff validate their paths (BAD_PATH) and size
    // them (NAME_TOO_LONG). A refused CRONTAB leaves the state at GET_CMD, so
    // its CRONLINEs are bad commands; nothing is taken.
    let long_dir = format!("/home/atlas/crons/{}", "d".repeat(237 - 18));
    let long_file = format!("/home/atlas/crons/{}", "f".repeat(252 - 18));
    let request = format!(
        "{}CRONTAB /etc/cron.d\nCRONLINE 240 1 1794447121 UNIFORM 3 evping -A \"1\" 192.0.2.1\n\
         CRONTAB /home/atlas/crons/../status\nCRONTAB {long_dir}\n\
         ONEOFF /tmp/oneoff evping -A \"1\" 192.0.2.1\nONEOFF {long_file} evping -A \"1\" 192.0.2.1\n\n",
        login()
    );
    let (out, commands) = session(&request).await;
    assert_eq!(
        out,
        "\r\nPassword: BAD_PATH\r\n\r\nBAD_COMMAND\r\n\r\nBAD_PATH\r\n\r\nNAME_TOO_LONG\r\n\r\n\
         BAD_PATH\r\n\r\nNAME_TOO_LONG\r\n\r\nOK\r\n\r\n"
    );
    assert!(commands.is_empty());
}

#[test]
fn path_rules_follow_validate_filename() {
    assert_eq!(
        rebased_crons_path_len("/home/atlas/crons/main"),
        Some("/var/spool/ripe-atlas/crons/main".len())
    );
    assert_eq!(
        rebased_crons_path_len("/var/spool/ripe-atlas/crons/7"),
        Some(29)
    );
    assert_eq!(rebased_crons_path_len("/home/atlas/crons/"), Some(28));
    for bad in [
        "/home/atlas/crons",
        "/home/atlas/cronsx/main",
        "/home/atlas/data/new/7",
        "home/atlas/crons/main",
        "/home/atlas/crons/../status",
        "/home/atlas/crons/main/..",
        "",
    ] {
        assert_eq!(rebased_crons_path_len(bad), None, "{bad}");
    }
    assert!(rebased_crons_path_len("/home/atlas/crons/main/..x").is_some());

    // The longest names that fit: 236 bytes of directory (246 rebased, plus
    // "/root.new" and a NUL), 251 of oneoff file (plus ".new" and a NUL).
    let dir = |n: usize| format!("/home/atlas/crons/{}", "d".repeat(n - 18));
    assert_eq!(crontab_dir_check(&dir(236)), Ok(()));
    assert_eq!(crontab_dir_check(&dir(237)), Err(NAME_TOO_LONG));
    let file = |n: usize| format!("/home/atlas/crons/{} evping 192.0.2.1", "f".repeat(n - 18));
    assert_eq!(oneoff_file_check(&file(251)), Ok(()));
    assert_eq!(oneoff_file_check(&file(252)), Err(NAME_TOO_LONG));
    // The name ends at any C white space
    assert_eq!(
        oneoff_file_check("/home/atlas/crons/oneoff\tevping"),
        Ok(())
    );
    assert_eq!(
        oneoff_file_check(" /home/atlas/crons/oneoff evping"),
        Err(BAD_PATH)
    );
}

#[tokio::test]
async fn reboot_and_reload_as_on_a_software_probe() {
    // ripe-atlas.service runs telnetd unprivileged: reboot(2) fails and
    // nothing is sent; RELOAD's remount fails, BAD_REMOUNT.
    let (out, commands) = session(&format!("{}REBOOT\nRELOAD\n\n", login())).await;
    assert_eq!(out, "\r\nPassword: BAD_REMOUNT\r\n\r\nOK\r\n\r\n");
    assert!(commands.is_empty());
}

#[tokio::test]
async fn starla_own_commands_get_bad_command_and_are_taken() {
    // The reference knows none of STATUS, STOP and JSON: each is a
    // BAD_COMMAND (telnetd.c:1223-1227), and one inside a CRONTAB ends it
    // first (:1241-1256) — the crontab, empty, is installed — so a CRONLINE
    // after it is a BAD_COMMAND too. Starla answers as the reference does
    // (it used to answer OK), and still takes its own.
    let json = r#"{"type":"ping","msm_id":1001,"target":"192.0.2.1"}"#;
    let request = format!(
        "{}CRONTAB /home/atlas/crons/main\nstatus\n\
         CRONLINE 240 1 1794447121 UNIFORM 3 evping -A \"1\" 192.0.2.1\n\
         STOP 1001\nSTOP x\n{json}\n{{\"type\":\"none\"}}\n\n",
        login()
    );
    let (out, commands) = session(&request).await;
    assert_eq!(
        out,
        format!("\r\nPassword: {}OK\r\n\r\n", BAD_COMMAND.repeat(6))
    );
    let mut wanted = vec![crontab("main", &[])];
    wanted.extend(["status", "STOP 1001", json].map(parse_command));
    assert_eq!(handed_on(&commands), handed_on(&wanted));
}

/// A session the test drives line by line, left open
struct Open {
    controller: tokio::io::DuplexStream,
    commands: mpsc::Receiver<TelnetCommand>,
    handler: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn open(session_id: &Arc<tokio::sync::RwLock<Option<String>>>) -> Open {
    let (controller, probe) = tokio::io::duplex(1 << 16);
    let (tx, commands) = mpsc::channel(16);
    let handler = tokio::spawn(handle_connection(
        probe,
        Some(tx),
        1018212,
        session_id.clone(),
    ));
    Open {
        controller,
        commands,
        handler,
    }
}

/// Log in and send a ONEOFF: once it is handed on, the session is in GET_CMD.
async fn authenticate(session: &mut Open) {
    let request = format!(
        "{}ONEOFF /home/atlas/crons/oneoff evping -A \"1\" 192.0.2.1\n",
        login()
    );
    session
        .controller
        .write_all(request.as_bytes())
        .await
        .unwrap();
    let command = session.commands.recv().await;
    assert!(
        matches!(&command, Some(c) if !matches!(c, TelnetCommand::Unknown(_))),
        "a known command"
    );
}

#[tokio::test]
async fn a_newer_session_takes_over() {
    // atlas_ts (telnetd.c:243-247): one controller session at a time. When
    // another authenticates, the older one is closed on telnetd's next pass,
    // input or not (:1143, :1234, :1266).
    let probe = Arc::new(tokio::sync::RwLock::new(Some(SID.to_string())));
    let another_probe = Arc::new(tokio::sync::RwLock::new(Some(SID.to_string())));
    let mut first = open(&probe);
    authenticate(&mut first).await;
    let mut elsewhere = open(&another_probe);
    authenticate(&mut elsewhere).await;
    let mut second = open(&probe);
    authenticate(&mut second).await;

    // The first ends with no further byte from its controller
    let mut out = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        first.controller.read_to_end(&mut out),
    )
    .await
    .expect("the older session is still open")
    .unwrap();
    assert!(out.ends_with(b"\r\nPassword: "));
    first.handler.await.unwrap().unwrap();
    assert!(first.commands.recv().await.is_none());

    // The newer one carries on, and so does another probe's
    for session in [&mut second, &mut elsewhere] {
        session.controller.write_all(b"\n").await.unwrap();
        session.controller.shutdown().await.unwrap();
        let mut out = Vec::new();
        session.controller.read_to_end(&mut out).await.unwrap();
        assert!(out.ends_with(b"\r\nPassword: OK\r\n\r\n"));
    }
}

/// The events this thread logs while a journal is open, each as the
/// journal prints it: the message, then any fields after it.
///
/// A subscriber of the whole test process that records per thread (a
/// #[tokio::test] runs its tasks on its own thread). A scoped default
/// subscriber would not do: a callsite that a test on another thread hits
/// first is given that thread's interest (none), for every thread.
struct Journal;

thread_local! {
    static JOURNAL: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

impl Journal {
    fn open() {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            tracing::subscriber::set_global_default(Journal).expect("no other subscriber");
        });
        JOURNAL.set(Some(Vec::new()));
    }

    fn close() -> Vec<String> {
        JOURNAL.take().expect("a journal is open")
    }

    /// Whether the open journal has `entry` yet
    fn has(entry: &str) -> bool {
        JOURNAL.with_borrow(|journal| {
            journal
                .as_ref()
                .expect("a journal is open")
                .iter()
                .any(|e| e == entry)
        })
    }
}

impl tracing::Subscriber for Journal {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        // Ask enabled() each time: it depends on the thread
        tracing::subscriber::Interest::sometimes()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        JOURNAL.with_borrow(Option::is_some)
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        #[derive(Default)]
        struct Line {
            message: String,
            fields: String,
        }
        impl tracing::field::Visit for Line {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                } else {
                    self.fields += &format!(" {}={value:?}", field.name());
                }
            }
        }
        let mut line = Line::default();
        event.record(&mut line);
        JOURNAL.with_borrow_mut(|journal| {
            if let Some(journal) = journal {
                journal.push(line.message + &line.fields);
            }
        });
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn request_lines_are_logged_as_the_twin_reads_them() {
    // The twin builds its scenarios from Starla's journal (twin/src/scenario.rs
    // `extract`): each "Telnet state Authenticated, received: '<line>'" is a
    // request line, and its replay sends them with an empty line after.
    let mut lines = crontab_request();
    lines.truncate(4);
    // Answered, not taken: replayed all the same, to the same answer
    lines.push("FOO bar".to_string());
    let request = format!(
        "C_TO_P_TEST_V1\nwrong\n{SID}\n{}\n\nafter the end\n",
        lines.join("\n")
    );
    Journal::open();
    let (out, _) = session(&request).await;
    let journal = Journal::close();
    assert_eq!(
        out,
        "\r\nPassword: BAD_PASSWORD\r\n\r\nBAD_COMMAND\r\n\r\nOK\r\n\r\nBAD_COMMAND\r\n\r\n"
    );

    let needle = "Telnet state Authenticated, received: '";
    let extracted: Vec<&str> = journal
        .iter()
        .filter_map(|m| m.find(needle).map(|i| &m[i + needle.len()..]))
        .map(|l| l.strip_suffix('\'').unwrap_or(l))
        .collect();
    // Exactly the request's lines: not its empty line, which the replay adds
    // (logged too, the replay would send two, and draw a BAD_COMMAND from
    // both probes), nor what follows it
    assert_eq!(extracted, lines, "{journal:#?}");
    assert_eq!(
        journal
            .iter()
            .filter(|m| *m == "Telnet: end of request")
            .count(),
        1
    );
    assert!(journal
        .iter()
        .any(|m| m == "Telnet: after the end of request, received: 'after the end'"));
    // And the session ID stays out of it
    assert!(journal
        .iter()
        .all(|m| !m.contains(SID) && !m.contains("wrong")));
}
