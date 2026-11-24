//! `ensure_daemon` : vérifie/installe/met à jour le démon sur un serveur
//! distant via SSH.
//!
//! Intégrité (I1 — revue v1) : l'ancien script faisait `curl | tar`
//! SUR le serveur — artefact jamais authentifié et hôte condamné à
//! joindre GitHub. Désormais (option (a) de la spec) l'app télécharge
//! l'artefact de la release elle-même, le vérifie — `SHA256SUMS` signé
//! minisign, `proto::release_verify` — puis le pousse dans le stdin du
//! script ssh : le serveur n'a plus besoin de GitHub ni de curl, et ce
//! qu'il installe est exactement ce qui a passé la signature sur le
//! poste de confiance. Une release sans manifeste (pré-signature) est
//! refusée ; `AGENTCONTROL_ALLOW_UNSIGNED=1` la laisse passer en
//! dépannage (warn), jamais une signature ou une empreinte invalide.
//!
//! Déroulé : sonde `check` (arch + version + port, zéro mutation) →
//! réponse directe `ok:`/`newer:`/`busy:` ou `install:<arch>` →
//! download + vérif côté client (étapes `download`/`integrity`) →
//! push stdin + déploiement (`push`/`install`/`service`/`verify`).

use client_core::Level;
use proto::release_verify;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

use crate::commands::pair::effective_hk;
use crate::state::split_user_host;

const RELEASES: &str = "https://github.com/Wameuh/AgentControl/releases";

/// Sonde distante : arch + version du démon présent + listener sur le
/// port — zéro mutation, zéro download. Dernière ligne stdout :
/// `ok:<ver>` | `newer:<ver>` | `busy:<addr>` | `managed:<detail>` |
/// `install:<arch>`. `{t}`/`{p}` = tag cible / port démon, `{f}` =
/// « forçage » (1 = l'utilisateur a confirmé « installer quand même »
/// après un `managed:` — les gardes managé sont alors sautées),
/// substitués côté Rust.
///
/// `ver` non vide = le démon répond — à jour quel que soit le
/// superviseur (systemd, supervisord, lancé à la main). Gardes avant
/// toute mutation : acctl via PATH en plus de ~/.local/bin (un démon
/// installé par image/conteneur rapporte alors sa version), et probe
/// du port daemon configuré — un listener ÉTRANGER sur ce port (démon
/// publié par Docker, port forwardé, autre service) ne doit jamais
/// être écrasé par l'artefact release. Le probe ne bloque pas notre
/// propre démon : `ver` non vide = le nôtre répond en socket unix ;
/// on re-sonde alors le port avec ce même acctl via AGENTCONTROL_TCP
/// — une version identique prouve que le listener est le sien (bind
/// TCP du même process) → l'upgrade continue ; une version différente
/// (autre démon managé ailleurs) ou aucune réponse (service étranger,
/// timeout 4 s si `timeout` existe) → busy. Le probe porte
/// AGENTCONTROL_TOKEN lu depuis daemon.token : sans lui un démon
/// authentifié (B1) répondrait « auth-required » → faux busy.
/// (acctl relit aussi le fichier lui-même — redondance par sécurité
/// pour un AGENTCONTROL_HOME non standard.)
///
/// Le probe du port n'exige plus curl (plus de download distant) :
/// curl quand il existe, /proc/net/tcp{,6} sinon — les deux absents
/// dégradent en « port libre » plutôt qu'en échec sec.
/// `rc=0; cmd || rc=$?` car `set -e` tue le shell sur tout échec de
/// commande — `rc=$?` seul ne s'exécuterait jamais.
/// `echo step:<id> >&2` : marqueurs streamés → events `daemon-step`
/// (stdout garde le résultat métier en dernière ligne).
///
/// Garde « managé ailleurs » (incident v0.11.0-rc.2 BUG C) : port
/// libre + aucun démon joignable ne suffit PAS à conclure `install:` —
/// un démon peut être géré par un conteneur/superviseur momentanément
/// down, et l'install dépose une unit systemd sur l'hôte de prod.
/// Avant `install:` la sonde cherche les TRACES d'une telle gestion
/// (~/.agentcontrol, clé `agentcontrol*`/`permitopen` dans
/// authorized_keys, conteneur/projet docker, compose publiant le port)
/// → `managed:<detail>` : le client affiche l'avertissement et
/// n'installe qu'après confirmation explicite (`{f}`=1). Toutes les
/// détections docker/compose sont optionnelles (`command -v`,
/// `2>/dev/null`) — une machine sans docker dégrade proprement.
const PROBE_SCRIPT: &str = "set -e; echo step:check >&2; arch=$(uname -m); \
    case $arch in x86_64|aarch64) ;; *) echo \"unsupported arch: $arch\" >&2; exit 1;; esac; \
    bin=$HOME/.local/bin/agentcontrol-daemon; \
    ac=$(command -v acctl 2>/dev/null || true); [ -z \"$ac\" ] && [ -x \"$HOME/.local/bin/acctl\" ] && ac=\"$HOME/.local/bin/acctl\"; \
    ver=$({ [ -n \"$ac\" ] && \"$ac\" status 2>/dev/null || true; } | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
    if [ \"$ver\" = \"{t}\" ]; then echo ok:$ver; exit 0; fi; \
    if [ -n \"$ver\" ] && [ \"$ver\" != \"{t}\" ] && \
       [ \"$(printf '%s\\n%s\\n' \"{t}\" \"$ver\" | sed 's/-rc\\./~rc./g' | sort -V | tail -n1)\" = \"$(echo \"$ver\" | sed 's/-rc\\./~rc./g')\" ]; then \
       echo newer:$ver; exit 0; fi; \
    rc=7; \
    if command -v curl >/dev/null 2>&1; then \
    rc=0; curl -s -o /dev/null --max-time 2 http://127.0.0.1:{p} 2>/dev/null || rc=$?; \
    elif [ -r /proc/net/tcp ]; then \
    hx=$(printf '%04X' {p}); \
    if awk 'FNR>1 && $4==\"0A\" { split($2,a,\":\"); if (a[2]==hx) f=1 } END{ exit !f }' hx=$hx /proc/net/tcp /proc/net/tcp6 2>/dev/null; then rc=0; fi; \
    fi; \
    if [ \"$rc\" -ne 7 ]; then \
    own=\"\"; \
    if [ -n \"$ver\" ]; then \
    to=$(command -v timeout >/dev/null 2>&1 && echo \"timeout 4\" || true); \
    own=$({ AGENTCONTROL_TCP=127.0.0.1:{p} AGENTCONTROL_TOKEN=$(cat \"${AGENTCONTROL_HOME:-$HOME/.agentcontrol}/daemon.token\" 2>/dev/null || true) $to \"$ac\" status 2>/dev/null || true; } | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
    fi; \
    if [ -z \"$own\" ] || [ \"$own\" != \"$ver\" ]; then echo busy:127.0.0.1:{p}; exit 0; fi; \
    fi; \
    if [ \"{f}\" != \"1\" ]; then \
    mg=\"\"; \
    ah=\"${AGENTCONTROL_HOME:-$HOME/.agentcontrol}\"; \
    [ -d \"$ah\" ] && mg=\"agentcontrol-home\"; \
    if [ -z \"$mg\" ] && [ -f \"$HOME/.ssh/authorized_keys\" ] && \
       grep -Eq 'permitopen=[^ ]*127\\.0\\.0\\.1:{p}| agentcontrol' \"$HOME/.ssh/authorized_keys\" 2>/dev/null; then mg=\"authorized-keys\"; fi; \
    if [ -z \"$mg\" ] && command -v docker >/dev/null 2>&1; then \
    docker ps -a --format '{{.Names}} {{.Image}}' 2>/dev/null | grep -qiE 'agentcontrol|devcontainer' && mg=\"docker\"; \
    if [ -z \"$mg\" ]; then docker compose ls 2>/dev/null | grep -qi agentcontrol && mg=\"docker-compose\"; fi; \
    fi; \
    if [ -z \"$mg\" ]; then \
    for cf in \"$HOME\"/dev/*/.devcontainer/compose.yml \"$HOME\"/dev/*/compose.yml; do \
    [ -f \"$cf\" ] && grep -qw '{p}' \"$cf\" 2>/dev/null && { mg=\"compose-file\"; break; }; \
    done; \
    fi; \
    if [ -n \"$mg\" ]; then echo \"managed:$mg\"; exit 0; fi; \
    fi; \
    echo install:$arch";

/// Déploiement distant : l'artefact arrive VÉRIFIÉ sur stdin
/// (`cat > acd.tgz` — poussé par l'app après signature + empreinte
/// contrôlées, I1) puis install + superviseur + probe final — section
/// inchangée du script historique, hors download qui vit désormais
/// côté client. Dernière ligne stdout : `installed:<ver>`.
const INSTALL_SCRIPT: &str = "set -e; \
    d=$HOME/.cache/agentcontrol-upd; rm -rf \"$d\"; mkdir -p \"$d\"; cd \"$d\" || exit 1; \
    bin=$HOME/.local/bin/agentcontrol-daemon; \
    echo step:push >&2; cat > acd.tgz; \
    echo step:install >&2; tar xzf acd.tgz; \
    install -Dm755 agentcontrol-daemon $bin; \
    install -Dm755 acctl $HOME/.local/bin/acctl; \
    for f in acp-*; do [ -f \"$f\" ] && install -Dm755 \"$f\" \"$HOME/.local/bin/$f\"; done; \
    echo step:service >&2; \
    confs=\"/etc/supervisor/conf.d/*.conf /etc/supervisord.d/*.conf $HOME/.supervisor/conf.d/*.conf\"; \
    ef=\"\"; for f in $confs; do [ -f \"$f\" ] && ef=\"$ef $f\"; done; \
    prog=$({ [ -n \"$ef\" ] && awk 'FNR==1 { p=\"\" } /^\\[/ { p=\"\"; if ($0 ~ /^\\[program:/) { p=$0; sub(/.*\\[program:/,\"\",p); sub(/\\].*/,\"\",p) } } /^command[[:space:]]*=/ && /agentcontrol-daemon/ && p != \"\" { print p; exit }' $ef 2>/dev/null || true; }); \
    if [ -n \"$prog\" ]; then \
    svc=$(grep -h '^command[[:space:]]*=' $ef 2>/dev/null | grep agentcontrol-daemon | head -n1 | sed 's/^command[[:space:]]*=[[:space:]]*//' | cut -d' ' -f1); \
    svc=$(command -v \"$svc\" 2>/dev/null || echo \"$svc\"); \
    case $svc in /*/agentcontrol-daemon) ;; *) svc=\"\";; esac; \
    if [ -n \"$svc\" ] && [ \"$svc\" != \"$bin\" ] && [ -w \"$(dirname \"$svc\")\" ]; then install -Dm755 agentcontrol-daemon \"$svc\"; fi; \
    if [ -n \"$svc\" ] && { [ \"$svc\" = \"$bin\" ] || [ -w \"$(dirname \"$svc\")\" ]; }; then supervisorctl restart \"$prog\" || true; fi; \
    elif command -v systemctl >/dev/null 2>&1 && systemctl cat agentcontrol-daemon.service >/dev/null 2>&1; then \
    :; \
    elif command -v systemctl >/dev/null 2>&1 && systemctl --user list-units >/dev/null 2>&1; then \
    install -Dm644 packaging/agentcontrol-daemon.service $HOME/.config/systemd/user/agentcontrol-daemon.service; \
    systemctl --user daemon-reload; \
    systemctl --user enable agentcontrol-daemon; \
    pkill -x agentcontrol-da 2>/dev/null || true; sleep 1; \
    systemctl --user restart agentcontrol-daemon; \
    else \
    pkill -x agentcontrol-da 2>/dev/null || true; sleep 2; \
    pgrep -x agentcontrol-da >/dev/null 2>&1 || nohup \"$bin\" >/dev/null 2>&1 </dev/null & \
    fi; \
    rm -rf \"$d\"; \
    echo step:verify >&2; \
    v=\"\"; for i in 1 2 3; do \
    v=$($HOME/.local/bin/acctl status 2>/dev/null | grep -o '\"version\":\"[^\"]*\"' | cut -d'\"' -f4); \
    [ -n \"$v\" ] && break; sleep 1; done; \
    echo installed:$v";

/// Étape de progression : event `daemon-step` pour le dialogue UI +
/// ligne client.log (une install lancée par une autre instance reste
/// corrélable au flap transport).
fn step(app: &AppHandle, host: &str, s: &str) {
    client_core::log(Level::Info, format!("ensure_daemon {host}: step {s}"));
    let _ = app.emit("daemon-step", s.to_string());
}

/// Dernière ligne non vide — le résultat métier du script distant
/// (`ok:*`/`newer:*`/`busy:*`/`install:*`/`installed:*`).
fn last_line(out: &str) -> String {
    out.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Trace chaque exec distant : code + résultat + stderr collectée — en
/// cas d'échec le log garde la cause (ssh, droits, script) qui sinon
/// ne vivrait que dans le toast éphémère.
fn log_result(host: &str, code: i32, out: &str, err: &str, t0: &std::time::Instant) {
    client_core::log(
        if code == 0 { Level::Info } else { Level::Warn },
        format!(
            "ensure_daemon {host}: code={code} out={out} err={} ({} ms)",
            if err.is_empty() { "-" } else { err },
            t0.elapsed().as_millis()
        ),
    );
}

/// Comment un `ssh <script>` s'exécute : binaire OpenSSH (stdin =
/// fichier local poussé par le shell) ou canal russh (stdin = octets
/// lus du même fichier — `ssh_exec_stdin`). Mêmes credentials que le
/// transport de connexion.
enum SshRun {
    OpenSsh {
        host: String,
        args: Vec<String>,
    },
    Native {
        host: String,
        port: u16,
        user: String,
        password: Option<String>,
        key: Option<PathBuf>,
        opts: client_core::ssh_native::Opts,
    },
}

/// Exécute `script` sur l'hôte — `stdin_path` donné = le fichier est
/// poussé comme entrée standard (`cat > acd.tgz` côté script), sinon
/// stdin null. Les marqueurs `step:*` de stderr partent en events
/// `daemon-step` ; retour = (code, stdout trim, stderr résiduelle trim).
async fn run_script(
    app: &AppHandle,
    host_label: &str,
    run: &SshRun,
    script: &str,
    stdin_path: Option<&Path>,
    timeout_secs: u64,
) -> Result<(i32, String, String), String> {
    match run {
        SshRun::OpenSsh { host, args } => {
            let mut cmd = tokio::process::Command::new("ssh");
            cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
                .args(args)
                .arg(host)
                .arg(script)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            match stdin_path {
                // L'artefact vérifié file en stdin — `ssh <tgz` : le
                // serveur ne voit que des octets déjà authentifiés.
                Some(p) => {
                    let f = std::fs::File::open(p).map_err(|e| format!("{} : {e}", p.display()))?;
                    cmd.stdin(std::process::Stdio::from(f));
                }
                None => {
                    cmd.stdin(std::process::Stdio::null());
                }
            }
            client_core::hide_console(&mut cmd);
            let mut child = cmd.spawn().map_err(|e| format!("ssh: {e}"))?;
            // stderr streamé : marqueurs → events, le reste collecté
            // pour l'erreur. kill_on_drop tue le ssh si le timeout le
            // lâche.
            let stderr_task = {
                let app2 = app.clone();
                let host2 = host_label.to_string();
                let pipe = child.stderr.take();
                tauri::async_runtime::spawn(async move {
                    let mut buf = String::new();
                    if let Some(p) = pipe {
                        use tokio::io::AsyncBufReadExt;
                        let mut lines = tokio::io::BufReader::new(p).lines();
                        loop {
                            match lines.next_line().await {
                                Ok(Some(l)) => {
                                    if let Some(s) = l.trim().strip_prefix("step:") {
                                        step(&app2, &host2, s);
                                    } else {
                                        buf.push_str(&l);
                                        buf.push('\n');
                                    }
                                }
                                Ok(None) => break,
                                // Ligne non-UTF8 : le drain s'arrête
                                // mais la coupure est tracée (et dans
                                // le buffer rendu).
                                Err(e) => {
                                    client_core::log(
                                        Level::Warn,
                                        format!(
                                            "ensure_daemon {host2}: drain stderr interrompu: {e}"
                                        ),
                                    );
                                    buf.push_str(&format!("(stderr unreadable: {e})\n"));
                                    break;
                                }
                            }
                        }
                    }
                    buf
                })
            };
            let out = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                child.wait_with_output(),
            )
            .await
            .map_err(|_| format!("ssh ensure_daemon timeout ({timeout_secs} s)"))?
            .map_err(|e| format!("ssh: {e}"))?;
            let stderr = stderr_task.await.unwrap_or_default();
            Ok((
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
                stderr.trim().to_string(),
            ))
        }
        SshRun::Native {
            host,
            port,
            user,
            password,
            key,
            opts,
        } => {
            let err_acc = Mutex::new(String::new());
            let app2 = app.clone();
            let host2 = host_label.to_string();
            let mut on_line = |line: &str| {
                if let Some(s) = line.trim().strip_prefix("step:") {
                    step(&app2, &host2, s);
                } else {
                    let mut g = err_acc.lock().unwrap();
                    g.push_str(line);
                    g.push('\n');
                }
            };
            let r = match stdin_path {
                Some(p) => {
                    let data = std::fs::read(p).map_err(|e| format!("{} : {e}", p.display()))?;
                    client_core::ssh_native::ssh_exec_stdin(
                        host,
                        *port,
                        user,
                        password.as_deref(),
                        key.as_deref(),
                        script,
                        &data,
                        Some(&mut on_line),
                        opts,
                        std::time::Duration::from_secs(timeout_secs),
                    )
                    .await
                }
                None => {
                    client_core::ssh_native::ssh_exec(
                        host,
                        *port,
                        user,
                        password.as_deref(),
                        key.as_deref(),
                        script,
                        Some(&mut on_line),
                        opts,
                    )
                    .await
                }
            };
            let (code, out, _err) = r.map_err(|e| format!("{e:#}"))?;
            Ok((
                code,
                out.trim().to_string(),
                err_acc.into_inner().unwrap_or_default().trim().to_string(),
            ))
        }
    }
}

/// GET d'un asset de release — `Ok(None)` sur 404 (asset absent du tag
/// → repli `latest` par l'appelant), `Err` sur toute autre panne.
fn http_asset(url: &str, timeout_secs: u64) -> Result<Option<ureq::Response>, String> {
    match ureq::get(url)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .set("User-Agent", "agentcontrol-ensure-daemon")
        .call()
    {
        Ok(r) => Ok(Some(r)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(e) => Err(format!("GET {url} : {e}")),
    }
}

/// URLs candidates d'un asset de release : tag exact d'abord (les rc
/// sont des prereleases — `latest` renverrait la stable), `latest` en
/// repli — même convention que le curl historique.
fn release_urls(tag: &str, name: &str) -> [String; 2] {
    [
        format!("{RELEASES}/download/v{tag}/{name}"),
        format!("{RELEASES}/latest/download/{name}"),
    ]
}

/// Télécharge l'asset `name` de la release `tag` vers `dest` — `Err`
/// si absent du tag ET de latest.
fn fetch_release_file(tag: &str, name: &str, dest: &Path) -> Result<(), String> {
    const CAP: u64 = 512 * 1024 * 1024;
    for url in release_urls(tag, name) {
        if let Some(resp) = http_asset(&url, 300)? {
            use std::io::Read;
            let mut f = std::fs::File::create(dest).map_err(|e| e.to_string())?;
            // CAP+1 lu : un dépassement devient un Err explicite plutôt
            // qu'une troncature silencieuse (le sha256 rattraperait de
            // toute façon — la cause est juste plus claire ici).
            let mut r = resp.into_reader().take(CAP + 1);
            std::io::copy(&mut r, &mut f).map_err(|e| e.to_string())?;
            if f.metadata().map_err(|e| e.to_string())?.len() > CAP {
                return Err(format!("{name} exceeds {CAP} bytes — abnormal artifact"));
            }
            return Ok(());
        }
    }
    Err(format!("{name} missing from release v{tag} (and latest)"))
}

/// Texte d'un asset release — `Ok(None)` s'il n'existe sur aucune des
/// deux URLs (release pré-signature sans manifeste, I1).
fn fetch_release_text(tag: &str, name: &str) -> Result<Option<String>, String> {
    for url in release_urls(tag, name) {
        if let Some(resp) = http_asset(&url, 30)? {
            return resp.into_string().map(Some).map_err(|e| e.to_string());
        }
    }
    Ok(None)
}

/// Phase 2 : l'artefact `agentcontrol-server-<arch>-linux.tar.gz` est
/// téléchargé ICI — poste de confiance — puis vérifié contre le
/// manifeste signé de la même release AVANT tout envoi. Retourne le
/// chemin du tarball à pousser ; le dossier temp est purgé en cas
/// d'échec (et par l'appelant après l'install).
async fn fetch_verified_daemon(
    app: &AppHandle,
    host: &str,
    tag: &str,
    arch: &str,
) -> Result<PathBuf, String> {
    step(app, host, "download");
    let name = format!("agentcontrol-server-{arch}-linux.tar.gz");
    let dir = std::env::temp_dir().join(format!("agentcontrol-dl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tgz = dir.join(&name);
    let (tag, host) = (tag.to_string(), host.to_string());
    let (app2, tgz_c, dir_c) = (app.clone(), tgz.clone(), dir.clone());
    let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
        fetch_release_file(&tag, &name, &tgz_c)?;
        step(&app2, &host, "integrity");
        let sums = fetch_release_text(&tag, release_verify::SUMS_NAME)?;
        let sig = fetch_release_text(&tag, release_verify::SIG_NAME)?;
        match (sums, sig) {
            (Some(s), Some(g)) => {
                let f = std::fs::File::open(&tgz_c).map_err(|e| e.to_string())?;
                release_verify::verify(&name, f, &s, &g)
                    .map_err(|e| format!("{name} failed integrity check — {e}"))?;
                client_core::log(
                    Level::Info,
                    format!("ensure_daemon {host}: {name} signature minisign + sha256 vérifiés"),
                );
            }
            (None, None) if release_verify::allow_unsigned() => {
                client_core::log(
                    Level::Warn,
                    format!(
                        "ensure_daemon {host}: release v{tag} non signée — acceptée \
                         (AGENTCONTROL_ALLOW_UNSIGNED)"
                    ),
                );
            }
            (None, None) => {
                return Err(format!(
                    "unsigned release v{tag} — {} and {} missing (older releases: \
                     AGENTCONTROL_ALLOW_UNSIGNED=1 to force, not recommended)",
                    release_verify::SUMS_NAME,
                    release_verify::SIG_NAME
                ));
            }
            _ => {
                return Err(format!(
                    "incomplete integrity manifest — only one of {}/{} published \
                     (tampered release?)",
                    release_verify::SUMS_NAME,
                    release_verify::SIG_NAME
                ));
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Err(e) = res {
        let _ = std::fs::remove_dir_all(&dir_c);
        return Err(e);
    }
    Ok(tgz)
}

/// Vérifie/installe/maj le démon sur le serveur distant via SSH.
/// Sonde → artefact téléchargé + vérifié par l'app (SHA256SUMS signé
/// minisign, I1) → poussé dans le stdin du script d'install — le
/// serveur n'a plus besoin d'accéder à GitHub ni de curl. Tag exact
/// demandé, fallback "latest". Idempotent.
/// Retourne la réponse : `ok:*`/`newer:*`/`busy:*`/`managed:*` sont
/// rendus tels quels (aucune mutation) ; `install:*` déclenche le
/// déploiement.
/// `native` (auto sur Android) : exec via russh au lieu du binaire ssh.
/// `allow_managed` : l'utilisateur a confirmé « installer quand même »
/// après un `managed:*` — la sonde saute alors les gardes « managé
/// ailleurs » et peut conclure `install:*`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn ensure_daemon(
    app: AppHandle,
    host: String,
    args: Option<Vec<String>>,
    tag: String,
    port: Option<u16>,
    remote_port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    key: Option<String>,
    native: Option<bool>,
    host_keys: Option<Vec<String>>,
    allow_managed: Option<bool>,
) -> Result<String, String> {
    let t0 = std::time::Instant::now();
    client_core::log(Level::Info, format!("ensure_daemon {host} tag={tag}"));
    // `tag` vient de la release GitHub (rel.tag_name) → attacker-
    // controlled sous le modèle I1 (compte/PAT compromis). Il part en
    // interpolation shell dans PROBE_SCRIPT exécuté AVANT toute
    // signature, et en URL — whitelist semver stricte en premier.
    if !release_verify::valid_tag(&tag) {
        return Err(format!("invalid release tag {tag:?}"));
    }
    let native = native.unwrap_or(false) || cfg!(target_os = "android");
    let run = if native {
        let (user, h) = split_user_host(&host, user.as_deref())?;
        let port = port.unwrap_or(22);
        // Profil appairé → la host key est épinglée pour les execs ;
        // known_hosts sert d'épinglage initial (TOFU —
        // `hostkey-unknown` sinon, traité par l'UI comme une première
        // connexion).
        let opts = client_core::ssh_native::Opts {
            expected_hk: effective_hk(host_keys, &h, port).await.map(Arc::new),
            ..Default::default()
        };
        SshRun::Native {
            host: h,
            port,
            user,
            password,
            key: key.map(PathBuf::from),
            opts,
        }
    } else {
        SshRun::OpenSsh {
            host: host.clone(),
            args: args.unwrap_or_default(),
        }
    };
    // Phase 1 — sonde distante (zéro mutation) : le dernier token
    // stdout tranche — réponse directe ou feu vert pour la suite.
    let probe = PROBE_SCRIPT
        .replace("{t}", &tag)
        .replace("{p}", &remote_port.unwrap_or(7331).to_string())
        .replace("{f}", if allow_managed.unwrap_or(false) { "1" } else { "0" });
    let (code, out, err) = run_script(&app, &host, &run, &probe, None, 120).await?;
    let last = last_line(&out);
    log_result(&host, code, &last, &err, &t0);
    if code != 0 {
        return Err(if err.is_empty() { last } else { err });
    }
    let Some(arch) = last.strip_prefix("install:").map(str::to_string) else {
        return Ok(last); // ok:* / newer:* / busy:* / managed:*
    };
    // Phase 2 — artefact téléchargé + vérifié côté app (I1) : tout ce
    // qui partira vers le serveur a déjà passé la signature.
    let tgz = fetch_verified_daemon(&app, &host, &tag, &arch).await?;
    // Phase 3 — push stdin + install + superviseur + probe final.
    let res = run_script(&app, &host, &run, INSTALL_SCRIPT, Some(&tgz), 300).await;
    let _ = std::fs::remove_dir_all(tgz.parent().unwrap_or(tgz.as_path()));
    let (code, out, err) = res?;
    let last = last_line(&out);
    log_result(&host, code, &last, &err, &t0);
    if code != 0 {
        return Err(if err.is_empty() { last } else { err });
    }
    Ok(last)
}
