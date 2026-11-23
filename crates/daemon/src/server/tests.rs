use super::{expand_cwd, expand_env, home_of, resolve_cwd};

#[test]
fn env_simple_vars() {
    std::env::set_var("AC_EXPAND_T1", "/v1");
    assert_eq!(expand_env("$AC_EXPAND_T1/x"), "/v1/x");
    assert_eq!(expand_env("${AC_EXPAND_T1}/x"), "/v1/x");
    assert_eq!(expand_env("pre$AC_EXPAND_T1"), "pre/v1");
}

#[test]
fn env_bornes() {
    // Inconnu, formes non-var et `${` non refermé : littéral, jamais
    // de shell. La valeur injectée n'est pas rescanée (pas de
    // récursion : T2 vaut "$AC_EXPAND_T1" affiché tel quel).
    std::env::set_var("AC_EXPAND_T2", "$AC_EXPAND_T1");
    assert_eq!(expand_env("$NOPE_X_123"), "$NOPE_X_123");
    assert_eq!(expand_env("$1"), "$1");
    assert_eq!(expand_env("$(id)"), "$(id)");
    assert_eq!(expand_env("${"), "${");
    assert_eq!(expand_env("$AC_EXPAND_T2"), "$AC_EXPAND_T1");
}

#[test]
fn tilde_home() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(expand_cwd("~").unwrap(), home);
    assert_eq!(expand_cwd("~/x").unwrap(), format!("{home}/x"));
    assert_eq!(expand_cwd("$HOME/x").unwrap(), format!("{home}/x"));
}

#[test]
#[cfg(unix)]
fn tilde_user() {
    // root existe partout ; un user improbable refuse proprement.
    // (pas de "/root" en dur : /var/root sur macOS)
    let root_home = home_of("root").unwrap().to_string_lossy().to_string();
    assert_eq!(expand_cwd("~root").unwrap(), root_home);
    assert_eq!(expand_cwd("~root/x").unwrap(), format!("{root_home}/x"));
    assert!(expand_cwd("~user_nope_zz9").is_err());
}

#[test]
fn resolve_valide_et_erreur_claire() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(resolve_cwd("~").unwrap(), home);
    let e = resolve_cwd("~/dir_nope_zz9").unwrap_err().to_string();
    assert!(e.contains("dir_nope_zz9"), "{e}");
    assert!(e.contains("expanded from"), "{e}");
    // Forme déjà absolue : pas de mention « depuis ».
    let e2 = resolve_cwd("/dir_nope_zz9").unwrap_err().to_string();
    assert!(!e2.contains("from"), "{e2}");
}

fn advisor_profile(models: &[&str]) -> super::AgentProfile {
    super::AgentProfile {
        name: "T".into(),
        commands: vec![],
        via: super::AgentVia::Builtin,
        env: Default::default(),
        compact: Default::default(),
        max_instances: 0,
        catalogue_cmd: None,
        model_override_cmd: None,
        extra_models: vec![],
        advisor_models: models.iter().map(|s| s.to_string()).collect(),
        provision_skills: true,
    }
}

fn advisor_meta(advisor: Option<&str>) -> proto::SessionMeta {
    proto::SessionMeta {
        id: "s-t".into(),
        agent: "t".into(),
        title: String::new(),
        cwd: String::new(),
        created_at: String::new(),
        native_session_id: None,
        mode: None,
        model: None,
        model_source: None,
        model_override_sid: None,
        supports_load: None,
        available_modes: vec![],
        available_commands: vec![],
        available_models: vec![],
        advisor: advisor.map(str::to_string),
        advisor_models: vec![],
        config_options: vec![],
        pending_config: Default::default(),
        desired_config: Default::default(),
        state: "idle".into(),
        group: None,
        spawned_by: None,
        origin: None,
        context_used: None,
        context_size: None,
        last_error: None,
        wait: None,
    }
}

#[test]
fn advisor_meta_json() {
    use super::advisor_acp_meta;
    // Profil sans advisor_models → jamais d'injection.
    assert!(advisor_acp_meta(&advisor_profile(&[]), &advisor_meta(Some("opus"))).is_none());
    // advisor non posé → rien (défaut du profil/agent).
    assert!(advisor_acp_meta(&advisor_profile(&["opus"]), &advisor_meta(None)).is_none());
    // Valeur → advisorModel via extraArgs.settings.
    let v = advisor_acp_meta(&advisor_profile(&["opus"]), &advisor_meta(Some("fable"))).unwrap();
    assert_eq!(
        v["claudeCode"]["options"]["extraArgs"]["settings"],
        serde_json::json!("{\"advisorModel\":\"fable\"}")
    );
    // "off" → désactivation explicite par env.
    let v = advisor_acp_meta(&advisor_profile(&["opus"]), &advisor_meta(Some("off"))).unwrap();
    assert_eq!(
        v["claudeCode"]["options"]["env"]["CLAUDE_CODE_DISABLE_ADVISOR_TOOL"],
        "1"
    );
}

// --- Connexion NDJSON : ligne invalide, borne, cross-protocol ----

use super::{is_http_request_line, next_bounded_line, Daemon, MAX_LINE_BYTES};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt, AsyncWriteExt, BufReader};

/// Daemon minimal en tempdir — aucun listener ouvert, aucune
/// session : juste de quoi servir une connexion NDJSON sur duplex.
async fn daemon_test(tag: &str) -> Arc<Daemon> {
    let root = std::env::temp_dir().join(format!("ac-serveconn-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let (d, _resumable) = Daemon::open(
        root,
        HashMap::new(),
        super::RetentionCfg::default(),
        super::A2aCfg::default(),
        Vec::new(),
        "127.0.0.1:0".into(),
        super::OrchCfg::default(),
        super::CompactCfg::default(),
        super::WatchdogCfg::default(),
        false,
        HashMap::new(),
        std::sync::Arc::from("test-token"),
    )
    .await
    .unwrap();
    d
}

/// Première ligne HTTP (« METHOD cible HTTP/v ») — signature
/// byte-level du garde cross-protocol.
#[test]
fn http_request_line_detection() {
    assert!(is_http_request_line(b"GET / HTTP/1.1"));
    assert!(is_http_request_line(b"POST /api/x HTTP/2"));
    assert!(is_http_request_line(b"OPTIONS * HTTP/1.1"));
    assert!(is_http_request_line(b"CONNECT h:443 HTTP/1.1"));
    assert!(!is_http_request_line(b"{\"id\":1,\"req\":\"x\"}"));
    // Pas de cible séparée par un espace.
    assert!(!is_http_request_line(b"GET /HTTP/1.1"));
    assert!(!is_http_request_line(b"GET / HTTP/"));
    // Méthode hors liste / casse → pas une request line reconnue :
    // retombe sur le chemin « ligne non-JSON » (erreur puis close).
    assert!(!is_http_request_line(b"PRI * HTTP/2.0"));
    assert!(!is_http_request_line(b"get / http/1.1"));
    assert!(!is_http_request_line(b""));
}

/// La borne de ligne : au-delà de MAX_LINE_BYTES la lecture échoue
/// sans lire le reste ; exactement à la borne elle passe. '\n' et
/// '\r' finaux retirés, queue de flux sans '\n' rendue telle quelle.
#[tokio::test]
async fn bounded_line_cap() {
    // Ligne énorme sans '\n' → erreur (le fragment reste dans buf :
    // un appel Err = connexion morte, buf n'est pas réutilisé).
    let big = vec![b'x'; MAX_LINE_BYTES + 8];
    let mut buf = Vec::new();
    assert!(next_bounded_line(&mut BufReader::new(&big[..]), &mut buf)
        .await
        .is_err());
    // Pareil avec le '\n' tout à la fin : la borne frappe avant.
    let mut big2 = vec![b'x'; MAX_LINE_BYTES + 8];
    big2.push(b'\n');
    assert!(
        next_bounded_line(&mut BufReader::new(&big2[..]), &mut Vec::new())
            .await
            .is_err()
    );
    // Exactement à la borne + '\n' → ligne complète.
    let mut ok = vec![b'y'; MAX_LINE_BYTES];
    ok.push(b'\n');
    let l = next_bounded_line(&mut BufReader::new(&ok[..]), &mut Vec::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(l.len(), MAX_LINE_BYTES);
    // « a\r\nb » : '\r' retiré, queue sans '\n' rendue puis EOF.
    let mut r = BufReader::new(&b"a\r\nb"[..]);
    let mut buf = Vec::new();
    assert_eq!(
        next_bounded_line(&mut r, &mut buf).await.unwrap().unwrap(),
        b"a"
    );
    assert_eq!(
        next_bounded_line(&mut r, &mut buf).await.unwrap().unwrap(),
        b"b"
    );
    assert!(next_bounded_line(&mut r, &mut buf).await.unwrap().is_none());
}

/// Une requête HTTP sur le socket NDJSON : la connexion se ferme
/// SANS répondre — le corps ne doit jamais être exécuté comme RPC.
#[tokio::test]
async fn serve_conn_ferme_sur_http() {
    let d = daemon_test("http").await;
    let (client, server) = tokio::io::duplex(64 * 1024);
    let serve = {
        let d = Arc::clone(&d);
        tokio::spawn(async move { d.serve_conn(server, "tcp 127.0.0.1:9", None).await })
    };
    let (mut rd, mut wr) = tokio::io::split(client);
    wr.write_all(b"POST / HTTP/1.1\r\nHost: x\r\n\r\n{\"id\":1,\"req\":\"daemon_status\"}\n")
        .await
        .unwrap();
    let r = tokio::time::timeout(std::time::Duration::from_secs(5), serve)
        .await
        .expect("serve_conn stuck")
        .expect("serve_conn task");
    assert!(r.is_ok(), "{r:?}");
    // Aucune réponse émise — surtout pas d'« ok:true » pour le corps
    // JSON qui suivait les en-têtes HTTP.
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut v = Vec::new();
        rd.read_to_end(&mut v).await.map(|_| v)
    })
    .await
    .expect("eof")
    .unwrap();
    assert!(
        out.is_empty(),
        "réponse inattendue: {}",
        String::from_utf8_lossy(&out)
    );
}

/// Un JSON valide de `req` inconnue : erreur parse portant l'id, et
/// la connexion reste ouverte (compat client plus récent).
#[tokio::test]
async fn serve_conn_req_inconnue_reste_ouverte() {
    let d = daemon_test("unknown").await;
    let (client, server) = tokio::io::duplex(64 * 1024);
    let serve = {
        let d = Arc::clone(&d);
        tokio::spawn(async move { d.serve_conn(server, "unix#1", None).await })
    };
    let (rd, mut wr) = tokio::io::split(client);
    let mut rd = BufReader::new(rd);
    wr.write_all(b"{\"id\":42,\"req\":\"no_such_req\"}\n")
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), rd.read_line(&mut line))
        .await
        .expect("timeout")
        .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], 42);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap_or_default().contains("parse"),
        "{v}"
    );
    // Toujours ouverte : une vraie requête obtient sa réponse.
    wr.write_all(b"{\"id\":7,\"req\":\"list_sessions\"}\n")
        .await
        .unwrap();
    line.clear();
    tokio::time::timeout(std::time::Duration::from_secs(5), rd.read_line(&mut line))
        .await
        .expect("timeout")
        .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], 7);
    assert_eq!(v["ok"], true);
    drop(wr);
    drop(rd);
    tokio::time::timeout(std::time::Duration::from_secs(5), serve)
        .await
        .expect("serve_conn stuck")
        .expect("serve_conn task")
        .unwrap();
}

/// Une ligne non-JSON (autre qu'HTTP) : l'erreur parse part d'abord,
/// puis la connexion se ferme.
#[tokio::test]
async fn serve_conn_ligne_non_json_ferme() {
    let d = daemon_test("garbage").await;
    let (client, server) = tokio::io::duplex(64 * 1024);
    let serve = {
        let d = Arc::clone(&d);
        tokio::spawn(async move { d.serve_conn(server, "tcp 127.0.0.1:10", None).await })
    };
    let (rd, mut wr) = tokio::io::split(client);
    let mut rd = BufReader::new(rd);
    wr.write_all(b"pas du json\n").await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), rd.read_line(&mut line))
        .await
        .expect("timeout")
        .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], 0);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap_or_default().contains("parse"),
        "{v}"
    );
    // Puis fermeture : EOF côté client, serve_conn retourné propre.
    line.clear();
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), rd.read_line(&mut line))
        .await
        .expect("timeout")
        .unwrap();
    assert_eq!(n, 0, "{line}");
    tokio::time::timeout(std::time::Duration::from_secs(5), serve)
        .await
        .expect("serve_conn stuck")
        .expect("serve_conn task")
        .unwrap();
}
