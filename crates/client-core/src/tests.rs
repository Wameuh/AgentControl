use super::*;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ac-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn connect_fail_stage1() {
    let ssh = Transport::Ssh {
        host: "h".into(),
        remote_port: 7331,
        args: vec![],
    };
    let native = Transport::SshNative {
        host: "h".into(),
        port: 22,
        user: "u".into(),
        remote_port: 7331,
        password: None,
        key_path: None,
        expected_hk: None,
        accept_unknown_hostkey: false,
    };
    let unix = Transport::Unix("/tmp/s.sock".into());
    let tcp = Transport::Tcp {
        addr: "127.0.0.1:1".into(),
        token: None,
    };

    // ssh CLI : TOUT échec étage 1 = transport (auth, DNS, timeout…).
    for m in [
        "ssh exited (255) : Permission denied (publickey,password)",
        "ssh exited (255) : Could not resolve hostname h",
        "tunnel ssh h timeout",
        "spawn ssh — is openssh installed?",
    ] {
        assert!(
            classify_connect_stage1(&ssh, m).starts_with("connect-failed:transport:"),
            "{m}"
        );
    }
    // russh : auth et timeout = transport ; direct-tcpip refusé = absent.
    assert!(classify_connect_stage1(&native, "password refused")
        .starts_with("connect-failed:transport:"));
    assert!(classify_connect_stage1(&native, "ssh connect timeout")
        .starts_with("connect-failed:transport:"));
    assert!(classify_connect_stage1(
        &native,
        "direct-tcpip — is the daemon listening on the remote port?"
    )
    .starts_with("connect-failed:daemon-absent:"));
    assert!(classify_connect_stage1(&native, "direct-tcpip timeout")
        .starts_with("connect-failed:daemon-unresponsive:"));
    // unix/tcp : rien n'écoute = absent.
    assert!(classify_connect_stage1(
        &unix,
        "unix /tmp/s.sock: No such file or directory (os error 2)"
    )
    .starts_with("connect-failed:daemon-absent:"));
    assert!(
        classify_connect_stage1(&tcp, "tcp 127.0.0.1:1: Connection refused (os error 111)")
            .starts_with("connect-failed:daemon-absent:")
    );
    // auth-required : le démon a répondu — échec d'authentification,
    // distinct de « démon absent » et de l'échec transport (B1).
    assert!(classify_connect_stage1(&tcp, "auth-required").starts_with("connect-failed:auth:"));
    // hostkey-mismatch / hostkey-unknown : contrat TOFU — tels quels.
    assert_eq!(
        classify_connect_stage1(&native, "hostkey-mismatch:SHA256:abc"),
        "hostkey-mismatch:SHA256:abc"
    );
    assert_eq!(
        classify_connect_stage1(&native, "hostkey-unknown:SHA256:def"),
        "hostkey-unknown:SHA256:def"
    );
}

#[test]
fn connect_fail_stage2() {
    let ssh = Transport::Ssh {
        host: "h".into(),
        remote_port: 7331,
        args: vec![],
    };
    let native = Transport::SshNative {
        host: "h".into(),
        port: 22,
        user: "u".into(),
        remote_port: 7331,
        password: None,
        key_path: None,
        expected_hk: None,
        accept_unknown_hostkey: false,
    };
    let unix = Transport::Unix("/tmp/s.sock".into());

    // ssh -L, port démon refusé : stderr « connect failed:
    // Connection refused » → absent (cas install).
    let tail = "channel 2: open failed: connect failed: Connection refused";
    assert!(classify_connect_stage2(
        &ssh,
        "connection closed",
        "forwarded channel closed — ssh process still alive",
        tail
    )
    .starts_with("connect-failed:daemon-absent:"));
    // Politique de forward : « open failed » sans refused → transport.
    assert!(classify_connect_stage2(
        &ssh,
        "connection closed",
        "forwarded channel closed — ssh process still alive",
        "channel 2: open failed: administratively prohibited: open failed"
    )
    .starts_with("connect-failed:transport:"));
    // EOF sans signature → transport.
    assert!(
        classify_connect_stage2(&ssh, "connection closed", "remote eof", "")
            .starts_with("connect-failed:transport:")
    );
    // Timeout 60 s = port qui écoute sans répondre → unresponsive.
    assert!(classify_connect_stage2(&ssh, "daemon timeout", "", "")
        .starts_with("connect-failed:daemon-unresponsive:"));
    // russh : canal direct-tcpip refermé session vivante → absent.
    assert!(classify_connect_stage2(
        &native,
        "connection closed",
        "direct-tcpip channel closed — ssh session still alive",
        ""
    )
    .starts_with("connect-failed:daemon-absent:"));
    // unix : socket accepté puis EOF → absent.
    assert!(
        classify_connect_stage2(&unix, "connection closed", "remote eof", "")
            .starts_with("connect-failed:daemon-absent:")
    );
    // Le détail est borné ~400 c.
    let big = "x".repeat(2000);
    let out = classify_connect_stage1(&ssh, &big);
    assert!(out.starts_with("connect-failed:transport:"));
    assert!(out.len() < 460, "{}", out.len());
}

#[test]
fn req_classifiers() {
    // Lectures mutualisables ; mutations jamais dédupliquées.
    assert!(req_dedupable("list_sessions"));
    assert!(req_dedupable("daemon_status"));
    assert!(!req_dedupable("send"));
    assert!(!req_dedupable("new_session"));
    assert!(!req_dedupable("delete_session"));
    assert!(!req_dedupable("term_open"));
    // Grosses réponses = heavy ; sondes et petits RPC = light.
    assert!(req_heavy("list_sessions"));
    assert!(req_heavy("get_blob"));
    assert!(!req_heavy("daemon_status"));
    assert!(!req_heavy("session_info"));
    assert!(!req_heavy("send"));
}

#[test]
fn level_parse_and_order() {
    assert_eq!(Level::parse("WARN"), Some(Level::Warn));
    assert_eq!(Level::parse(" warning "), Some(Level::Warn));
    assert_eq!(Level::parse("err"), Some(Level::Error));
    assert_eq!(Level::parse("nope"), None);
    assert!(Level::Debug < Level::Info && Level::Info < Level::Warn && Level::Warn < Level::Error);
}

#[test]
fn rotation_shifts_and_bounds() {
    let d = tmpdir("rot");
    let log = d.join("client.log");
    for i in 0..5 {
        std::fs::write(&log, format!("gen {i}\n")).unwrap();
        rotate_logs(&d, 3);
    }
    // .1/.2/.3 existent, .4 a été supprimé → contenu borné
    assert_eq!(
        std::fs::read_to_string(d.join("client.log.1")).unwrap(),
        "gen 4\n"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("client.log.3")).unwrap(),
        "gen 2\n"
    );
    assert!(!d.join("client.log.4").exists());
}

#[test]
fn writer_rotates_at_max() {
    let d = tmpdir("wmax");
    let mut w = LogWriter::open(d.clone());
    let line = "x".repeat(1024);
    // ~1.5 MiB → doit déclencher une rotation
    for _ in 0..1500 {
        w.write_line(&line);
    }
    assert!(d.join("client.log.1").exists());
    assert!(std::fs::metadata(d.join("client.log")).unwrap().len() < LOG_MAX_BYTES + 2048);
    // l'écriture continue après rotation
    w.write_line("apres rotation");
    let tail = std::fs::read_to_string(d.join("client.log")).unwrap();
    assert!(tail.contains("apres rotation"));
}
