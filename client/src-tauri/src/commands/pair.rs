//! Appairage d'un appareil par QR code.
//!
//! Protocole principal B′ (forced-command SSH à clé jetable) :
//!   le desktop installe sur le serveur une ligne authorized_keys
//!   restreinte à `pair.sh <nonce> <rport>` authentifiée par une clé
//!   jetable K (v≤5 min, one-shot), puis affiche un QR v2.1
//!   `{"v":2,host,port,user,rport,k:<PEM K>,hk:[SHA256 fps],exp}` —
//!   `hk` épingle les host keys du serveur (MITM tué au handshake) ;
//!   l'appareil s'authentifie avec K, la forced command installe SA clé
//!   propre (`restrict,port-forwarding,permitopen=…`), K s'auto-détruit.
//!
//! Repli A (opt-in averti) : `agentcontrol://pair?v=1&…&secret=<b64>`
//!   partage le credential du profil — passe-partout permanent, gardé
//!   pour les serveurs où authorized_keys n'est pas inscriptible.
//! Mode dégradé H : QR d'endpoint seul `{v:2,host,port,user,rport}` —
//!   la pubkey de l'appareil est posée à la main.
//!
//! Le payload QR et `k` ne vont jamais dans les logs.

use base64::Engine;
use client_core::Level;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::state::split_user_host;

/// Capacité byte-mode d'un QR ~lisible (niveau EC L, version 40 =
/// 2953 o — marge pour le schéma et les paramètres). Au-delà : refus
/// net plutôt qu'un QR trop dense pour la caméra.
const URI_MAX: usize = 2800;

/// pair.sh embarqué (forced command durcie) — versionné par le
/// marqueur `# ac-pair-version: N`, réinstallé si plus vieux.
const PAIR_SH: &str = include_str!("../../pair.sh");
/// Version attendue — l'install réécrit pair.sh si < PAIR_SH_VERSION.
const PAIR_SH_VERSION: u32 = 4;
/// Fenêtre de vie de K côté serveur (heure serveur) — audit F-K1 : ≤5 min.
const PAIR_TTL_S: u64 = 300;

/// base64url sans padding — compact et sûr dans une URI query.
fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// `~` → HOME/USERPROFILE en tête de chemin (clés du profil / args -i).
fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(h).join(rest);
        }
    }
    PathBuf::from(p)
}

/// `-p <port>` / `-p<port>` dans les args du ssh CLI (port sshd réel
/// d'une connexion non-native — cfg.port y est un résidu de formulaire).
fn args_port(args: &[String]) -> Option<u64> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-p" {
            if let Some(p) = it.next().and_then(|s| s.parse().ok()) {
                return Some(p);
            }
        } else if let Some(p) = a.strip_prefix("-p").and_then(|s| s.parse().ok()) {
            return Some(p);
        }
    }
    None
}

/// `-i <clé>` / `-i<clé>` dans les args du ssh CLI.
fn args_key_path(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-i" {
            if let Some(p) = it.next() {
                return Some(p.clone());
            }
        } else if let Some(p) = a.strip_prefix("-i") {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

fn cfg_args(cfg: &Value) -> Vec<String> {
    cfg.get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// PEM d'un fichier clé si lisible ET valide — Option : les candidats
/// s'enchaînent, l'appelant décide si aucun n'a abouti.
fn try_key_pem(path: &str) -> Option<String> {
    client_core::ssh_native::read_key_pem(&expand_home(path)).ok()
}

// ---------------------------------------------------------------------------
// Exec d'un script POSIX sur le serveur — même chemin que ensure_daemon :
// natif → russh ssh_exec (pinning hk du profil si présent) ;
// CLI → `ssh <args> <host> <script>` (BatchMode).
// ---------------------------------------------------------------------------

async fn exec_via_cfg(cfg: &Value, script: &str) -> Result<(i32, String, String), String> {
    let native = cfg.get("native").and_then(Value::as_bool).unwrap_or(false)
        || cfg!(target_os = "android");
    if native {
        let host_raw = cfg.get("host").and_then(Value::as_str).unwrap_or_default();
        let (user, host) = split_user_host(
            host_raw,
            cfg.get("user").and_then(Value::as_str),
        )?;
        let opts = client_core::ssh_native::Opts {
            expected_hk: cfg
                .get("host_keys")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<HashSet<_>>()
                })
                .filter(|s| !s.is_empty())
                .map(Arc::new),
            key: None,
        };
        let key_path = cfg
            .get("key")
            .and_then(Value::as_str)
            .map(|k| expand_home(k));
        return client_core::ssh_native::ssh_exec(
            &host,
            cfg.get("port").and_then(Value::as_u64).unwrap_or(22) as u16,
            &user,
            cfg.get("password").and_then(Value::as_str),
            key_path.as_deref(),
            script,
            None,
            &opts,
        )
        .await
        .map_err(|e| format!("{e:#}"));
    }
    let host = cfg
        .get("host")
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
        .ok_or("hôte manquant")?;
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
        .args(cfg_args(cfg))
        .arg(host)
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    client_core::hide_console(&mut cmd);
    let out = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| "ssh exec timeout".to_string())?
        .map_err(|e| format!("ssh: {e}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

// ---------------------------------------------------------------------------
// B′ — côté DESKTOP : résolution d'endpoint, install, poll, cleanup,
// liste/révocation des appareils appairés.
// ---------------------------------------------------------------------------

/// Endpoint SSH réel à afficher/éditer avant génération du QR.
/// Profil CLI : l'hôte peut être un alias ~/.ssh/config sans sens pour
/// le téléphone → résolution effective via `ssh -G` (honore aussi les
/// options -p/-F/-i des args du profil). Profil natif : champs tels quels.
#[tauri::command]
pub(crate) async fn pair_resolve(cfg: Value) -> Result<Value, String> {
    if cfg.get("kind").and_then(Value::as_str) != Some("ssh") {
        return Err("seules les connexions SSH sont appairables".into());
    }
    let native = cfg.get("native").and_then(Value::as_bool).unwrap_or(false)
        || cfg!(target_os = "android");
    if native {
        let host_raw = cfg.get("host").and_then(Value::as_str).unwrap_or_default();
        let (user, host) = split_user_host(
            host_raw,
            cfg.get("user").and_then(Value::as_str),
        )?;
        return Ok(json!({
            "host": host,
            "port": cfg.get("port").and_then(Value::as_u64).unwrap_or(22),
            "user": user,
        }));
    }
    let host = cfg
        .get("host")
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
        .ok_or("hôte manquant")?
        .to_string();
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.arg("-G").args(cfg_args(&cfg)).arg(&host)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    client_core::hide_console(&mut cmd);
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| "ssh -G timeout".to_string())?
        .map_err(|e| format!("ssh -G: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh -G a échoué : {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let val = |k: &str| {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(k))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let rhost = val("hostname ").ok_or("ssh -G : hostname introuvable")?;
    let port = val("port ").and_then(|p| p.parse::<u64>().ok()).unwrap_or(22);
    let user = val("user ").unwrap_or_default();
    Ok(json!({ "host": rhost, "port": port, "user": user }))
}

/// Install serveur : pair.sh versionné + ligne K sous verrou + état
/// d'expiration (heure serveur) + fps des host keys + nonce CSPRNG.
/// Sorties parsées : hk:<fp> (≥1 obligatoire), exp:<ts>, nonce:<hex>,
/// warn:<msg>, sweep:<mode>, err:<code>.
const INSTALL_SH: &str = r#"set -e
umask 077
PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin; export PATH
D="$HOME/.agentcontrol"; SD="$HOME/.ssh"; AK="$SD/authorized_keys"
mkdir -p "$D"; chmod 700 "$D"
mkdir -p "$SD"; chmod 700 "$SD"
if [ -L "$AK" ]; then echo "err:managed-ak"; exit 1; fi
if [ ! -f "$AK" ]; then : > "$AK"; chmod 600 "$AK"; fi

# Version du SSHD serveur uniquement — `ssh -V` donne celle du CLIENT,
# qui ne dit rien du support expiry-time du sshd local.
ver=$( (sshd -V 2>&1 || /usr/sbin/sshd -V 2>&1 || /sbin/sshd -V 2>&1 || true) | sed -n 's/.*OpenSSH_\([0-9][0-9.]*\).*/\1/p' | head -n 1)
has_exp=0
case "$ver" in
  ''|*[!0-9.]*) ;;
  *.*) maj=${ver%%.*}; rest=${ver#*.}; min=${rest%%.*}
    if [ "$maj" -gt 7 ] 2>/dev/null || { [ "$maj" -eq 7 ] && [ "$min" -ge 7 ] ; } 2>/dev/null; then has_exp=1; fi ;;
esac
[ -n "$ver" ] || echo "warn:sshd-version-unknown"

cur=$(sed -n 's/^# ac-pair-version: \([0-9]*\).*/\1/p' "$D/pair.sh" 2>/dev/null | head -n 1)
case "$cur" in ''|*[!0-9]*) cur=0 ;; esac
if [ "$cur" -lt @PVER@ ]; then
cat > "$D/pair.sh" <<'ACPAIRSH'
@PAIRSH@
ACPAIRSH
chmod 700 "$D/pair.sh"
fi

fps=""
for f in /etc/ssh/ssh_host_*_key.pub; do
  [ -f "$f" ] || continue
  fp=$(ssh-keygen -lf "$f" 2>/dev/null | awk '{print $2}')
  [ -n "$fp" ] && fps="$fps $fp"
done
set -- $fps
[ $# -gt 0 ] || { echo "err:nohk"; exit 1; }
for fp do echo "hk:$fp"; done

nonce=$(dd if=/dev/urandom bs=16 count=1 2>/dev/null | od -An -v -tx1 | tr -d '[:space:]')
exp=$(( $(date +%s) + @TTL@ ))

expopt=""
if [ "$has_exp" -eq 1 ]; then
  fmt=$(date -d "@$exp" +%Y%m%d%H%M%S 2>/dev/null || date -r "$exp" +%Y%m%d%H%M%S 2>/dev/null || date -j -f %s "$exp" +%Y%m%d%H%M%S 2>/dev/null || true)
  [ -n "$fmt" ] && expopt=",expiry-time=\"$fmt\""
fi

# Ménage opportuniste AVANT de prendre le verrou (pair.sh le reprend
# lui-même — depuis un shell héritant du fd 9 il attendrait 10 s pour
# rien). Échec toléré : le cron gère le long terme.
"$D/pair.sh" --sweep >/dev/null 2>&1 || true

if command -v flock >/dev/null 2>&1; then
  exec 9>"$D/pair.lock"
  flock -w 10 9 || { echo "err:lock"; exit 1; }
  trap 'exec 9>&-' EXIT
else
  i=0
  while ! mkdir "$D/pair.lock.d" 2>/dev/null; do
    i=$((i+1)); [ $i -gt 10 ] && { echo "err:lock"; exit 1; }
    p=$(cat "$D/pair.lock.d/pid" 2>/dev/null)
    { [ -n "$p" ] && ! kill -0 "$p" 2>/dev/null; } && rm -rf "$D/pair.lock.d" && continue
    sleep 1
  done
  echo $$ > "$D/pair.lock.d/pid"
  trap 'rm -rf "$D/pair.lock.d"' EXIT
fi
# Ligne K + état d'expiration écrits SOUS LE MÊME VERROU — atomique
# vis-à-vis de tout sweep (cron ou hook install) : un .exp sans ligne
# K serait balayé, orphelin ; une ligne sans état expirerait aussitôt.
printf 'restrict%s,command="%s/pair.sh %s %s" ssh-ed25519 %s agentcontrol-pair-tmp %s\n' \
  "$expopt" "$D" "$nonce" "@RPORT@" "@KB64@" "$nonce" >> "$AK"
printf '%s\n' "$exp" > "$D/pair-$nonce.exp"
chmod 600 "$D/pair-$nonce.exp"

if command -v crontab >/dev/null 2>&1; then
  (crontab -l 2>/dev/null | grep -v 'agentcontrol/pair.sh --sweep'; \
   echo "*/15 * * * * $D/pair.sh --sweep >/dev/null 2>&1") | crontab - 2>/dev/null \
    && echo "sweep:cron" || echo "sweep:inline-only"
else
  echo "sweep:inline-only"
fi
echo "exp:$exp"
echo "nonce:$nonce"
"#;

/// Retrait de la ligne K + état (succès, annulation, expiration vue) —
/// pair.sh est CONSERVÉ (permanent, versionné).
const CLEANUP_SH: &str = r#"D="$HOME/.agentcontrol"; AK="$HOME/.ssh/authorized_keys"
umask 077
# Symlink apparu entre l'install et le cleanup (setup managé) : ne pas
# remplacer le lien par un fichier régulier — on ne touche à rien.
if [ -L "$AK" ]; then rm -f "$D/pair-@NONCE@.exp" "$D/pair-@NONCE@.seen"; echo ok; exit 0; fi
if [ -f "$AK" ]; then
if command -v flock >/dev/null 2>&1; then
  exec 9>"$D/pair.lock"; flock -w 10 9 || exit 1
else
  i=0
  while ! mkdir "$D/pair.lock.d" 2>/dev/null; do
    i=$((i+1)); [ $i -gt 10 ] && exit 1
    sleep 1
  done
fi
cp -p "$AK" "$AK.bak" 2>/dev/null && chmod 600 "$AK.bak" 2>/dev/null
grep -v "agentcontrol-pair-tmp @NONCE@\$" "$AK" > "$AK.tmp.$$" 2>/dev/null
rc=$?
if [ "$rc" -le 1 ]; then
  chmod 600 "$AK.tmp.$$"; mv "$AK.tmp.$$" "$AK"
  command -v restorecon >/dev/null 2>&1 && restorecon "$AK" 2>/dev/null
else
  rm -f "$AK.tmp.$$"
fi
command -v flock >/dev/null 2>&1 && exec 9>&- || rm -rf "$D/pair.lock.d"
fi
rm -f "$D/pair-@NONCE@.exp" "$D/pair-@NONCE@.seen"
echo ok
"#;

/// État d'un appairage en cours : paired (pair-ok dans pair.log),
/// expired (état serveur passé), waiting (ligne K toujours posée),
/// gone (ligne disparue sans pair-ok — balayée / revoke / race perdue).
const POLL_SH: &str = r#"D="$HOME/.agentcontrol"; AK="$HOME/.ssh/authorized_keys"
L="$D/pair.log"; S="$D/pair-@NONCE@.exp"
if [ -f "$L" ] && awk -v n="@NONCE@" '$2==n && $3=="pair-ok" {print "paired:" ($5=="" ? "?" : $5); found=1; exit} END {exit !found}' "$L" 2>/dev/null; then
  :
elif [ -f "$S" ] && [ "$(date +%s)" -ge "$(cat "$S" 2>/dev/null || echo 0)" ] 2>/dev/null; then
  echo expired
elif [ -f "$AK" ] && grep -q "agentcontrol-pair-tmp @NONCE@\$" "$AK" 2>/dev/null; then
  echo waiting
else
  echo gone
fi
"#;

/// Liste des appareils appairés : les lignes `agentcontrol-pair ` sont
/// relues (fp réel via ssh-keygen -lf, label + ts du commentaire serveur).
const DEVICES_SH: &str = r#"AK="$HOME/.ssh/authorized_keys"; D="$HOME/.agentcontrol"
[ -f "$AK" ] || exit 0
grep ' agentcontrol-pair ' "$AK" 2>/dev/null | while IFS= read -r line; do
  set -- $line
  while [ $# -gt 0 ]; do
    if [ "${3:-}" = "agentcontrol-pair" ]; then
      ty=$1; b64=$2; shift 3
      printf '%s %s\n' "$ty" "$b64" > "$D/.pairdev.$$"
      fp=$(ssh-keygen -lf "$D/.pairdev.$$" 2>/dev/null | awk '{print $2}')
      rm -f "$D/.pairdev.$$"
      [ -n "$fp" ] && printf 'dev:%s\t%s\t%s\n' "$fp" "${1:-?}" "${2:-?}"
      break
    fi
    shift
  done
done
exit 0
"#;

/// Révocation par fingerprint exact — même procédure verrouillée
/// (temp+mv + .bak 600), match -xF de la ligne entière.
const REVOKE_SH: &str = r#"AK="$HOME/.ssh/authorized_keys"; D="$HOME/.agentcontrol"
umask 077
[ -f "$AK" ] || { echo "err:none"; exit 0; }
if command -v flock >/dev/null 2>&1; then
  exec 9>"$D/pair.lock"; flock -w 10 9 || { echo "err:lock"; exit 1; }
else
  i=0
  while ! mkdir "$D/pair.lock.d" 2>/dev/null; do
    i=$((i+1)); [ $i -gt 10 ] && { echo "err:lock"; exit 1; }
    p=$(cat "$D/pair.lock.d/pid" 2>/dev/null)
    { [ -n "$p" ] && ! kill -0 "$p" 2>/dev/null; } && rm -rf "$D/pair.lock.d" && continue
    sleep 1
  done
fi
tmp="$AK.tmp.$$"; : > "$tmp"; found=""
while IFS= read -r line; do
  keep=1
  case "$line" in
    *" agentcontrol-pair "*)
      set -- $line
      while [ $# -gt 0 ]; do
        if [ "${3:-}" = "agentcontrol-pair" ]; then
          printf '%s %s\n' "$1" "$2" > "$D/.pk.$$"
          fp=$(ssh-keygen -lf "$D/.pk.$$" 2>/dev/null | awk '{print $2}')
          rm -f "$D/.pk.$$"
          [ "$fp" = "@FP@" ] && keep=0
          break
        fi
        shift
      done ;;
  esac
  if [ $keep -eq 1 ]; then printf '%s\n' "$line" >> "$tmp"; else found=1; fi
done < "$AK"
if [ -z "$found" ]; then
  rm -f "$tmp"
  command -v flock >/dev/null 2>&1 && exec 9>&- || rm -rf "$D/pair.lock.d"
  echo "err:notfound"; exit 1
fi
cp -p "$AK" "$AK.bak" 2>/dev/null && chmod 600 "$AK.bak" 2>/dev/null
chmod 600 "$tmp"; mv "$tmp" "$AK"
command -v restorecon >/dev/null 2>&1 && restorecon "$AK" 2>/dev/null
command -v flock >/dev/null 2>&1 && exec 9>&- || rm -rf "$D/pair.lock.d"
echo "ok"
"#;

fn build_install(k_b64: &str, rport: u16) -> String {
    INSTALL_SH
        .replace("@PVER@", &PAIR_SH_VERSION.to_string())
        .replace("@TTL@", &PAIR_TTL_S.to_string())
        .replace("@RPORT@", &rport.to_string())
        .replace("@KB64@", k_b64)
        .replace("@PAIRSH@", PAIR_SH)
}

/// Vérifie un nonce rendu par le serveur (hex 32) — il est interpolé
/// dans les scripts suivants : charset strict, sinon refus net.
fn check_nonce(n: &str) -> Result<(), String> {
    if n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err("nonce serveur invalide".into())
    }
}

/// Empreintes des host keys connues du desktop pour `host`
/// (`ssh-keygen -l -F <host>` — couvre les entrées hashées) : le
/// cross-check contre les fps collectées serveur est best-effort.
async fn known_hosts_fps(host: String) -> HashSet<String> {
    tokio::task::spawn_blocking(move || known_hosts_fps_sync(&host))
        .await
        .unwrap_or_default()
}

fn known_hosts_fps_sync(host: &str) -> HashSet<String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let kh = home.join(".ssh/known_hosts");
    if !kh.exists() {
        return HashSet::new();
    }
    let out = std::process::Command::new("ssh-keygen")
        .args(["-l", "-F", host, "-f"])
        .arg(&kh)
        .output();
    let Ok(out) = out else { return HashSet::new() };
    if !out.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().find(|t| t.starts_with("SHA256:")))
        .map(String::from)
        .collect()
}

/// pair_begin : génère K (ed25519 mémoire), installe via la session SSH
/// du profil (natif ou CLI), renvoie le payload QR v2.1 + exp + nonce.
/// `host`/`port`/`user` = l'endpoint CONFIRMÉ affiché dans le dialogue
/// (résolu par pair_resolve, éventuellement édité par l'utilisateur) ;
/// l'install elle-même passe par la cfg du profil (alias CLI ok).
#[tauri::command]
pub(crate) async fn pair_begin(
    cfg: Value,
    host: String,
    port: u16,
    user: String,
) -> Result<Value, String> {
    if cfg.get("kind").and_then(Value::as_str) != Some("ssh") {
        return Err("seules les connexions SSH sont appairables".into());
    }
    if host.is_empty() || host.len() > 253 || host.bytes().any(|b| b.is_ascii_whitespace() || b == b'@' || b == b'/') {
        return Err("endpoint invalide".into());
    }
    if user.len() > 64 || user.bytes().any(|b| b.is_ascii_whitespace() || b == b'@') {
        return Err("user invalide".into());
    }
    let rport = cfg.get("remote_port").and_then(Value::as_u64).unwrap_or(7331) as u16;

    let (k_pem, k_pub) =
        client_core::ssh_native::gen_keypair_mem().map_err(|e| format!("{e:#}"))?;
    // "ssh-ed25519 <b64>" → la partie blob, seule interpolée dans le script.
    let k_b64 = k_pub.split_whitespace().nth(1).unwrap_or_default().to_string();
    if !k_b64.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')) {
        return Err("clé jetable illisible".into());
    }

    let script = build_install(&k_b64, rport);
    let (code, out, err) = exec_via_cfg(&cfg, &script).await?;
    client_core::log(
        if code == 0 { Level::Info } else { Level::Warn },
        format!("pair_begin install: code={code} err={}", err.trim()),
    );
    let first_err = out.lines().find_map(|l| l.strip_prefix("err:"));
    if let Some(e) = first_err {
        let msg = match e {
            "nohk" => "host keys introuvables sur le serveur — appairage impossible, utilisez le code endpoint seul".into(),
            "managed-ak" => "authorized_keys managé (symlink) — utilisez le mode manuel".into(),
            "lock" => "verrou authorized_keys indisponible — réessayez".into(),
            other => format!("install refusée par le serveur ({other})"),
        };
        return Err(msg);
    }
    if code != 0 {
        return Err(format!(
            "install a échoué (code {code}){}",
            if err.trim().is_empty() { String::new() } else { format!(" : {}", err.trim()) }
        ));
    }
    let hk: Vec<String> = out
        .lines()
        .filter_map(|l| l.strip_prefix("hk:").map(String::from))
        .collect();
    if hk.is_empty() {
        return Err("host keys introuvables sur le serveur — appairage refusé".into());
    }
    let exp = out
        .lines()
        .find_map(|l| l.strip_prefix("exp:"))
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let nonce = out
        .lines()
        .find_map(|l| l.strip_prefix("nonce:"))
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    check_nonce(&nonce)?;
    // Cross-check best-effort : les fps lues côté serveur vs le
    // known_hosts du desktop — divergence = avertissement, pas un refus.
    let known = known_hosts_fps(host.clone()).await;
    let warn = if !known.is_empty() && !hk.iter().any(|f| known.contains(f)) {
        Some("les empreintes du serveur diffèrent de votre known_hosts — vérifiez hors-bande".to_string())
    } else {
        out.lines()
            .find_map(|l| l.strip_prefix("warn:"))
            .map(|s| s.to_string())
    };
    let sweep = out
        .lines()
        .find_map(|l| l.strip_prefix("sweep:"))
        .unwrap_or("inline-only")
        .to_string();
    let payload = json!({
        "v": 2,
        "host": host,
        "port": port,
        "user": user,
        "rport": rport,
        "k": k_pem,
        "hk": hk,
        "exp": exp,
    });
    let payload = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    if payload.len() > URI_MAX {
        return Err("payload trop grand pour un QR".into());
    }
    Ok(json!({
        "payload": payload,
        "exp": exp,
        "nonce": nonce,
        "warn": warn,
        "sweep": sweep,
    }))
}

/// État courant de l'appairage `nonce` côté serveur.
/// → "waiting" | "expired" | "gone" | "paired:<label>"
#[tauri::command]
pub(crate) async fn pair_poll(cfg: Value, nonce: String) -> Result<String, String> {
    check_nonce(&nonce)?;
    let script = POLL_SH.replace("@NONCE@", &nonce);
    let (_code, out, _err) = exec_via_cfg(&cfg, &script).await?;
    Ok(out.lines().next().unwrap_or("gone").trim().to_string())
}

/// Retrait de la ligne K + état — succès ET annulation (le succès a
/// déjà auto-retiré la ligne : idempotent).
#[tauri::command]
pub(crate) async fn pair_cleanup(cfg: Value, nonce: String) -> Result<(), String> {
    check_nonce(&nonce)?;
    let script = CLEANUP_SH.replace("@NONCE@", &nonce);
    let (code, out, err) = exec_via_cfg(&cfg, &script).await?;
    client_core::log(
        if code == 0 { Level::Info } else { Level::Warn },
        format!("pair_cleanup: code={code} out={} err={}", out.trim(), err.trim()),
    );
    Ok(())
}

/// Appareils appairés (lignes `agentcontrol-pair `) : [{fp,label,ts}].
#[tauri::command]
pub(crate) async fn pair_devices(cfg: Value) -> Result<Value, String> {
    let (_code, out, _err) = exec_via_cfg(&cfg, DEVICES_SH).await?;
    let devs: Vec<Value> = out
        .lines()
        .filter_map(|l| l.strip_prefix("dev:"))
        .filter_map(|l| {
            let mut it = l.split('\t');
            Some(json!({
                "fp": it.next()?,
                "label": it.next()?,
                "ts": it.next()?,
            }))
        })
        .collect();
    Ok(Value::Array(devs))
}

/// Révocation d'un appareil par fingerprint exact (les labels peuvent
/// être ambigus) — session desktop de confiance, procédure verrouillée.
#[tauri::command]
pub(crate) async fn pair_revoke(cfg: Value, fp: String) -> Result<(), String> {
    if fp.len() > 64
        || !fp.starts_with("SHA256:")
        || !fp[7..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return Err("empreinte invalide".into());
    }
    let script = REVOKE_SH.replace("@FP@", &fp);
    let (_code, out, err) = exec_via_cfg(&cfg, &script).await?;
    if let Some(e) = out.lines().find_map(|l| l.strip_prefix("err:")) {
        return Err(match e {
            "notfound" => "appareil introuvable (déjà révoqué ?)".into(),
            "lock" => "verrou authorized_keys indisponible".into(),
            other => format!("révocation refusée ({other})"),
        });
    }
    client_core::log(Level::Info, format!("pair_revoke {fp}: {} {}", out.trim(), err.trim()));
    Ok(())
}

// ---------------------------------------------------------------------------
// B′ — côté APPAREIL qui scanne : tentative d'installation via K.
// ---------------------------------------------------------------------------

/// pair_attempt : connexion russh authentifiée par K (PEM du QR,
/// jamais persistée) épinglant `hk`, exec `agentcontrol-pair …` et
/// vérification `pair-ok:<fp>` == empreinte de la clé de l'appareil.
/// Retourne le fp confirmé. Erreurs mappées : auth K KO (QR expiré),
/// host key hors pinning (MITM), pair.sh refusé, fp incohérent.
#[tauri::command]
pub(crate) async fn pair_attempt(
    host: String,
    port: u16,
    user: String,
    k_pem: String,
    hk: Vec<String>,
    label: String,
) -> Result<String, String> {
    if hk.is_empty() {
        return Err("hk absent — appairage refusé".into());
    }
    let hk: HashSet<String> = hk.into_iter().collect();
    let label: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let label = if label.is_empty() { "device".to_string() } else { label };
    // Le PEM de K vit ici en mémoire — jamais persisté ; scrub best
    // effort du buffer avant le drop (usage unique, échec compris).
    let r = client_core::ssh_native::pair_attempt(&host, port, &user, &k_pem, hk, &label)
        .await
        .map_err(|e| format!("{e:#}"));
    // Scrub best-effort du buffer avant le drop (usage unique, échec
    // compris) — le PEM reste présent dans le QR rendu à l'écran, mais
    // ne doit pas traîner dans le process plus que nécessaire.
    let mut bytes = k_pem.into_bytes();
    bytes.iter_mut().for_each(|b| *b = 0);
    drop(bytes);
    r
}

// ---------------------------------------------------------------------------
// Repli A (opt-in averti) — URI v1 « profil complet ».
// ---------------------------------------------------------------------------

/// Construit l'URI d'appairage d'une connexion sauvegardée (kind ssh).
/// Erreurs : pas de secret exportable (auth par agent/config ssh —
/// le QR ne peut dérouler que ce que le fichier profil connaît), ou
/// payload trop grand pour un QR.
#[tauri::command]
pub(crate) fn pair_payload(cfg: Value) -> Result<String, String> {
    if cfg.get("kind").and_then(Value::as_str) != Some("ssh") {
        return Err("seules les connexions SSH sont partageables".into());
    }
    let host_raw = cfg.get("host").and_then(Value::as_str).unwrap_or_default();
    let user_f = cfg.get("user").and_then(Value::as_str);
    let (user, host) = split_user_host(host_raw, user_f)?;
    if host.is_empty() {
        return Err("hôte manquant".into());
    }
    let native = cfg.get("native").and_then(Value::as_bool).unwrap_or(false)
        || cfg!(target_os = "android");
    let args: Vec<String> = cfg_args(&cfg);
    // sshd port : natif → cfg.port ; CLI → -p des args en priorité (le
    // champ port conserve la valeur par défaut du formulaire).
    let port = if native {
        cfg.get("port").and_then(Value::as_u64)
    } else {
        args_port(&args).or_else(|| cfg.get("port").and_then(Value::as_u64))
    }
    .unwrap_or(22);
    let rport = cfg.get("remote_port").and_then(Value::as_u64).unwrap_or(7331);
    let password = cfg
        .get("password")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    // PEM : clé explicite → clé app (natif) → -i des args (CLI).
    let pem = cfg
        .get("key")
        .and_then(Value::as_str)
        .and_then(|k| try_key_pem(k))
        .or_else(|| {
            (native && {
                let p = client_core::ssh_native::key_path();
                p.exists()
            })
            .then(|| try_key_pem(&client_core::ssh_native::key_path().to_string_lossy()))
            .flatten()
        })
        .or_else(|| args_key_path(&args).and_then(|k| try_key_pem(&k)));

    let mut q = url::form_urlencoded::Serializer::new(String::new());
    q.append_pair("v", "1")
        .append_pair("host", &host)
        .append_pair("port", &port.to_string())
        .append_pair("rport", &rport.to_string())
        .append_pair("user", &user);
    match pem {
        Some(pem) => {
            q.append_pair("auth", "key")
                .append_pair("secret", &b64url(pem.as_bytes()));
            if let Some(pw) = password {
                q.append_pair("pw", &b64url(pw.as_bytes()));
            }
        }
        None => match password {
            Some(pw) => {
                q.append_pair("auth", "pw")
                    .append_pair("secret", &b64url(pw.as_bytes()));
            }
            None => {
                return Err(
                    "aucun secret exportable — auth par agent/config ssh : \
                     passez en SSH natif ou renseignez une clé/un mot de passe"
                        .into(),
                )
            }
        },
    }
    if let Some(c) = cfg.get("color").and_then(Value::as_str).filter(|c| !c.is_empty()) {
        q.append_pair("color", c);
    }
    let uri = format!("agentcontrol://pair?{}", q.finish());
    if uri.len() > URI_MAX {
        return Err("clé trop grande pour un QR — utilisez une clé ed25519".into());
    }
    Ok(uri)
}

/// Stocke la clé PEM reçue par appairage (validée, chmod 600) et
/// retourne le chemin à poser dans cfg.key du profil importé.
#[tauri::command]
pub(crate) fn pair_import_key(pem: String, name: String) -> Result<String, String> {
    client_core::ssh_native::import_paired_key(&pem, &name)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("{e:#}"))
}
