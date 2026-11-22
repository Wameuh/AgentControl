//! Intégration B1 : le port TCP exige `{"req":"auth","token":…}` en
//! première ligne non vide — mauvais/absent jeton → « auth-required »
//! puis fermeture ; timeout 10 s sur la ligne d'auth ; rotation via
//! `token_rotate` (unix) révoque les conns TCP ouvertes ; le socket
//! unix reste sans gate (confiance FS). Spawne un vrai démon sur un
//! AGENTCONTROL_HOME temporaire — unix seulement (daemon.sock).
#![cfg(unix)]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio::process::{Child, Command};

type TcpReader = BufReader<tokio::net::tcp::OwnedReadHalf>;
type TcpWriter = tokio::net::tcp::OwnedWriteHalf;

/// Démon de test : home temporaire + config.toml bind_tcp loopback,
/// process tué à la fin du test.
struct TestDaemon {
    home: PathBuf,
    addr: String,
    child: Child,
}

impl TestDaemon {
    async fn spawn(tag: &str) -> Self {
        let home = std::env::temp_dir().join(format!("ac-tcpauth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // Port libre : bind :0, lecture, drop — le démon re-bind aussitôt
        // (race acceptable en test ; un échec serait vu à l'étape poll).
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = free.local_addr().unwrap().to_string();
        drop(free);
        std::fs::write(home.join("config.toml"), format!("bind_tcp = \"{addr}\"\n")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentcontrol-daemon"))
            .env("AGENTCONTROL_HOME", &home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn daemon");
        let d = Self { home, addr, child };
        d.wait_ready().await;
        d
    }

    /// Le listener TCP monte avant la fin de l'init (chargement des
    /// sessions, scan agents) — on poll l'accept plutôt qu'un sleep,
    /// puis l'existence de daemon.sock (bind juste après).
    async fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match TcpStream::connect(&self.addr).await {
                Ok(s) => {
                    drop(s);
                    break;
                }
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("daemon TCP {} unreachable: {e}", self.addr),
            }
        }
        while Instant::now() < deadline {
            if UnixStream::connect(self.home.join("daemon.sock"))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("daemon.sock never bound in {}", self.home.display());
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.home.join("daemon.token"))
            .unwrap()
            .trim()
            .to_string()
    }

    async fn tcp(&self) -> (TcpReader, TcpWriter) {
        let s = TcpStream::connect(&self.addr).await.unwrap();
        let (r, w) = s.into_split();
        (BufReader::new(r), w)
    }

    /// Envoie une ligne brute sur la conn TCP (handshake d'auth ou
    /// requête), lit UNE réponse JSON. None = conn refermée / timeout.
    async fn tcp_roundtrip(r: &mut TcpReader, w: &mut TcpWriter, line: &str) -> Option<Value> {
        w.write_all(line.as_bytes()).await.ok()?;
        w.write_all(b"\n").await.ok()?;
        w.flush().await.ok()?;
        // (&mut r) : lines() consomme son receveur — une réemprunte
        // garde le BufReader réutilisable.
        let l = match tokio::time::timeout(Duration::from_secs(15), (&mut *r).lines().next_line())
            .await
        {
            Ok(Ok(Some(l))) => l,
            _ => return None,
        };
        serde_json::from_str(&l).ok()
    }

    /// Une requête brute sur le socket unix (pas de gate d'auth).
    async fn unix_call(&self, line: &str) -> Value {
        let s = UnixStream::connect(self.home.join("daemon.sock"))
            .await
            .unwrap();
        let (ur, mut uw) = s.into_split();
        let mut ur = BufReader::new(ur);
        uw.write_all(line.as_bytes()).await.unwrap();
        uw.write_all(b"\n").await.unwrap();
        uw.flush().await.unwrap();
        let l = tokio::time::timeout(Duration::from_secs(15), (&mut ur).lines().next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&l).unwrap()
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_auth_gate() {
    let d = TestDaemon::spawn("gate").await;
    // Le jeton attendu, lu côté « admin » (même machine que le démon).
    let token = d.token();
    assert_eq!(token.len(), 64, "daemon.token = 32 octets hex");

    // ── 1. Première ligne non-JSON : « auth-required » + fermeture.
    let (mut r, mut w) = d.tcp().await;
    w.write_all(b"hello\n").await.unwrap();
    w.flush().await.unwrap();
    let l = (&mut r).lines().next_line().await.unwrap().unwrap();
    let v: Value = serde_json::from_str(&l).unwrap();
    assert_eq!(v["error"], "auth-required", "non-JSON first line: {v}");
    // Fermeture juste après la réponse.
    assert!((&mut r).lines().next_line().await.unwrap().is_none());

    // ── 2. JSON valide mais requête ≠ auth : idem, id propagé.
    let (mut r, mut w) = d.tcp().await;
    let v = TestDaemon::tcp_roundtrip(&mut r, &mut w, r#"{"id":7,"req":"daemon_status"}"#)
        .await
        .unwrap();
    assert_eq!(v["id"], 7);
    assert_eq!(v["error"], "auth-required", "non-auth request: {v}");
    assert!((&mut r).lines().next_line().await.unwrap().is_none());

    // ── 3. Mauvais jeton : auth-required, pas d'oracle sur la valeur.
    let (mut r, mut w) = d.tcp().await;
    let v = TestDaemon::tcp_roundtrip(
        &mut r,
        &mut w,
        r#"{"id":1,"req":"auth","token":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
    )
    .await
    .unwrap();
    assert_eq!(v["error"], "auth-required", "wrong token: {v}");

    // ── 4. Ligne HTTP : fermeture silencieuse (pas de réponse).
    let (mut r, mut w) = d.tcp().await;
    w.write_all(b"GET / HTTP/1.1\n").await.unwrap();
    w.flush().await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), (&mut r).lines().next_line())
        .await
        .unwrap()
        .unwrap();
    assert!(got.is_none(), "http line should close silently");

    // ── 5. Auth timeout : conn ouverte sans ligne → le démon referme
    // sous ~10 s (constant AUTH_TIMEOUT). Lancé en tâche pour ne pas
    // rallonger le test.
    let addr = d.addr.clone();
    let timeout_probe = tokio::spawn(async move {
        let t0 = Instant::now();
        let s = TcpStream::connect(addr).await.unwrap();
        let (mut r, _w) = s.into_split();
        let mut buf = [0u8; 4096];
        // EOF = le démon a refermé (le timeout auth est côté serveur).
        while matches!(r.read(&mut buf).await, Ok(n) if n > 0) {}
        t0.elapsed()
    });

    // ── 6. Bon jeton : {ok:true} puis une vraie RPC répond.
    let (mut r, mut w) = d.tcp().await;
    let auth = json!({"id": 1, "req": "auth", "token": token.as_str()}).to_string();
    let v = TestDaemon::tcp_roundtrip(&mut r, &mut w, &auth)
        .await
        .unwrap();
    assert_eq!(v["ok"], true, "good token: {v}");
    let v = TestDaemon::tcp_roundtrip(&mut r, &mut w, r#"{"id":2,"req":"daemon_status"}"#)
        .await
        .unwrap();
    assert_eq!(v["ok"], true, "rpc after auth: {v}");
    assert_eq!(v["result"]["auth"]["tcp"], true, "cap gate: {v}");

    // ── 7. Socket unix : pas de gate — daemon_status direct.
    let v = d.unix_call(r#"{"id":1,"req":"daemon_status"}"#).await;
    assert_eq!(v["ok"], true, "unix conn without auth: {v}");

    // ── 8. Rotation : token_rotate émis depuis une conn TCP
    // authentifiée — la réponse sort AVANT la révocation (drain).
    let (mut rt, mut wt) = d.tcp().await;
    let auth2 = json!({"id": 1, "req": "auth", "token": token.as_str()}).to_string();
    let v = TestDaemon::tcp_roundtrip(&mut rt, &mut wt, &auth2)
        .await
        .unwrap();
    assert_eq!(v["ok"], true, "re-auth before rotate: {v}");
    let v = TestDaemon::tcp_roundtrip(&mut rt, &mut wt, r#"{"id":2,"req":"token_rotate"}"#)
        .await
        .expect("rotate response should reach its own conn");
    assert_eq!(v["ok"], true, "token_rotate over tcp: {v}");
    assert!(
        v["result"].get("token").is_none(),
        "token never in RPC response: {v}"
    );
    let new_token = d.token();
    assert_ne!(new_token, token, "daemon.token must change on rotate");
    // Les deux conns TCP authentifiées (étapes 6 et 8) sont révoquées.
    let eof2 = tokio::time::timeout(Duration::from_secs(5), (&mut rt).lines().next_line())
        .await
        .unwrap()
        .unwrap();
    assert!(eof2.is_none(), "rotating conn must be revoked too");
    let eof = tokio::time::timeout(Duration::from_secs(5), (&mut r).lines().next_line())
        .await
        .unwrap()
        .unwrap();
    assert!(eof.is_none(), "rotated conn must be revoked");
    // Nouvelle conn : ancien jeton refusé, nouveau accepté.
    let (mut r2, mut w2) = d.tcp().await;
    let old = json!({"id": 1, "req": "auth", "token": token.as_str()}).to_string();
    let v = TestDaemon::tcp_roundtrip(&mut r2, &mut w2, &old)
        .await
        .unwrap();
    assert_eq!(v["error"], "auth-required", "old token after rotate: {v}");
    let (mut r3, mut w3) = d.tcp().await;
    let new = json!({"id": 1, "req": "auth", "token": new_token.as_str()}).to_string();
    let v = TestDaemon::tcp_roundtrip(&mut r3, &mut w3, &new)
        .await
        .unwrap();
    assert_eq!(v["ok"], true, "new token after rotate: {v}");

    // ── Timeout auth (probe lancée en 5) : fermée entre ~9 et 15 s.
    let elapsed = timeout_probe.await.unwrap();
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed <= Duration::from_secs(15),
        "auth timeout ≈10 s, got {elapsed:?}"
    );
}
