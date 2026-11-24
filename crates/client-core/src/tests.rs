use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{broadcast, oneshot, watch, Mutex};

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

// --- dispatch_rx_line : parse paresseux + orphelins (item #367) -----------

/// Client minimal hors-ligne — dispatch_rx_line n'a besoin que de
/// pending/pushes/compteurs (pas de transport).
fn stub_client() -> Arc<Client> {
    let (dead_tx, dead) = watch::channel(false);
    Arc::new(Client {
        pending: Mutex::new(Inflight::default()),
        outq: std::sync::Mutex::new(OutQueue::default()),
        outq_notify: tokio::sync::Notify::new(),
        open_light: AtomicU64::new(0),
        open_heavy: AtomicU64::new(0),
        last_heavy_tx_ms: AtomicU64::new(0),
        next_id: AtomicU64::new(1),
        next_waiter: AtomicU64::new(1),
        pushes: broadcast::channel(512).0,
        dead,
        dead_tx,
        last_rx_ms: AtomicU64::new(now_ms()),
        desc: "test".into(),
        closed: AtomicBool::new(false),
        reader_abort: std::sync::OnceLock::new(),
        dead_info: std::sync::Mutex::new(None),
        guard: None,
        created_ms: AtomicU64::new(now_ms()),
        rx_lines: AtomicU64::new(0),
        tx_lines: AtomicU64::new(0),
        rx_bytes: AtomicU64::new(0),
        tx_bytes: AtomicU64::new(0),
        rx_partial: AtomicU64::new(0),
        last_tx_ms: AtomicU64::new(now_ms()),
        abandoned_calls: AtomicU64::new(0),
        late_responses: AtomicU64::new(0),
        parsed_responses: AtomicU64::new(0),
        me: std::sync::OnceLock::new(),
    })
}

fn entry(waiters: Vec<(u64, WaiterTx)>, dedup_key: Option<&str>) -> InflightEntry {
    InflightEntry {
        dedup_key: dedup_key.map(String::from),
        class: ReqClass::Heavy,
        written: true,
        waiters,
    }
}

/// Réponse géante (> GIANT_RX_LINE_BYTES) sans aucune entrée pending :
/// la sonde lit id/ok mais `result` n'est JAMAIS matérialisé —
/// parsed_responses reste à 0, late compte le gaspillage évité.
#[tokio::test]
async fn dispatch_giant_response_unknown_id_skips_parse() {
    let c = stub_client();
    let big = "x".repeat(300 * 1024);
    let line = format!(r#"{{"id":77,"ok":true,"result":{{"blob":"{big}"}}}}"#);
    assert!(line.len() > GIANT_RX_LINE_BYTES);
    dispatch_rx_line(&c, line.as_bytes()).await;
    assert_eq!(c.late_responses.load(Ordering::Relaxed), 1);
    assert_eq!(c.parsed_responses.load(Ordering::Relaxed), 0);
}

/// Même ligne géante avec un waiter en vol : une SEULE matérialisation
/// du result, livrée en Arc au waiter.
#[tokio::test]
async fn dispatch_giant_response_with_waiter_parses_once() {
    let c = stub_client();
    let (tx, rx) = oneshot::channel();
    {
        let mut m = c.pending.lock().await;
        m.by_id.insert(9, entry(vec![(1, tx)], None));
    }
    c.open_heavy.store(1, Ordering::Relaxed);
    let big = "y".repeat(300 * 1024);
    let line = format!(r#"{{"id":9,"ok":true,"result":{{"blob":"{big}"}}}}"#);
    dispatch_rx_line(&c, line.as_bytes()).await;
    assert_eq!(c.parsed_responses.load(Ordering::Relaxed), 1);
    assert_eq!(c.late_responses.load(Ordering::Relaxed), 0);
    // Réponse consommée : compteur open soldé.
    assert_eq!(c.open_heavy.load(Ordering::Relaxed), 0);
    let v = rx.await.unwrap().unwrap();
    assert_eq!(v["blob"].as_str().unwrap().len(), big.len());
}

/// Entrée orpheline (waiters vides, grace ORPHAN_GRACE) : la réponse
/// arrive mais personne n'attend — pas de parse `result` non plus.
#[tokio::test]
async fn dispatch_orphan_response_skips_parse() {
    let c = stub_client();
    {
        let mut m = c.pending.lock().await;
        m.by_id.insert(10, entry(vec![], Some("k")));
        m.by_key.insert("k".into(), 10);
    }
    c.open_heavy.store(1, Ordering::Relaxed);
    let big = "z".repeat(300 * 1024);
    let line = format!(r#"{{"id":10,"ok":true,"result":{{"blob":"{big}"}}}}"#);
    dispatch_rx_line(&c, line.as_bytes()).await;
    assert_eq!(c.late_responses.load(Ordering::Relaxed), 1);
    assert_eq!(c.parsed_responses.load(Ordering::Relaxed), 0);
    // L'entrée est soldée à la réponse (parsing sauté, compteur ok).
    let m = c.pending.lock().await;
    assert!(!m.by_id.contains_key(&10));
    assert!(!m.by_key.contains_key("k"));
    drop(m);
    assert_eq!(c.open_heavy.load(Ordering::Relaxed), 0);
}

/// Waiter abandonné sur un appel dédupliquable : l'entrée survit
/// (orpheline) — un retry identique se ré-attache via by_key et
/// reçoit la réponse en vol au lieu de relancer une requête lourde.
#[tokio::test]
async fn settle_waiter_keeps_dedup_orphan_for_reattach() {
    let c = stub_client();
    let (tx1, _rx1) = oneshot::channel();
    {
        let mut m = c.pending.lock().await;
        m.by_id.insert(5, entry(vec![(1, tx1)], Some("k")));
        m.by_key.insert("k".into(), 5);
    }
    c.open_heavy.store(1, Ordering::Relaxed);
    {
        let mut m = c.pending.lock().await;
        settle_waiter(&mut m, &c, 5, 1);
    }
    assert_eq!(c.abandoned_calls.load(Ordering::Relaxed), 1);
    {
        let m = c.pending.lock().await;
        // Orpheline conservée : ni by_id ni by_key purgés, compteur
        // open Heavy intact (la requête est toujours sur le wire).
        assert!(m.by_id.contains_key(&5));
        assert_eq!(m.by_key.get("k"), Some(&5));
        assert!(m.by_id[&5].waiters.is_empty());
    }
    assert_eq!(c.open_heavy.load(Ordering::Relaxed), 1);
    // Retry : nouveau waiter sur la même entrée → la réponse en vol
    // est livrée (une seule matérialisation).
    let (tx2, rx2) = oneshot::channel();
    {
        let mut m = c.pending.lock().await;
        m.by_id.get_mut(&5).unwrap().waiters.push((2, tx2));
    }
    dispatch_rx_line(&c, br#"{"id":5,"ok":true,"result":{"prs":[]}}"#.as_slice()).await;
    let v = rx2.await.unwrap().unwrap();
    assert_eq!(v["prs"].as_array().unwrap().len(), 0);
    assert_eq!(c.parsed_responses.load(Ordering::Relaxed), 1);
}

/// Waiter abandonné sur un appel NON dédupliquable : retrait
/// immédiat (personne ne peut se ré-attacher — pas de dedup_key).
#[tokio::test]
async fn settle_waiter_drops_non_dedup_entry() {
    let c = stub_client();
    let (tx, _rx) = oneshot::channel();
    {
        let mut m = c.pending.lock().await;
        m.by_id.insert(6, entry(vec![(1, tx)], None));
    }
    c.open_heavy.store(1, Ordering::Relaxed);
    {
        let mut m = c.pending.lock().await;
        settle_waiter(&mut m, &c, 6, 1);
    }
    let m = c.pending.lock().await;
    assert!(!m.by_id.contains_key(&6));
    drop(m);
    assert_eq!(c.open_heavy.load(Ordering::Relaxed), 0);
}

/// Un push NDJSON est toujours matérialisé et broadcasté tel quel.
#[tokio::test]
async fn dispatch_push_broadcasts_value() {
    let c = stub_client();
    let mut rx = c.pushes.subscribe();
    dispatch_rx_line(
        &c,
        br#"{"push":"event","session":"s-1","event":{"seq":3,"ts":"t","type":"system","text":"hi"}}"#
            .as_slice(),
    )
    .await;
    let v = rx.try_recv().unwrap();
    assert_eq!(v["session"], "s-1");
    assert_eq!(v["event"]["seq"], 3);
}
