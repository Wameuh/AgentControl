// ---------------------------------------------------------------------------
// SSH natif (russh) — Android + tout hôte sans binaire ssh.
//
// Transports directs : canal `direct-tcpip` vers 127.0.0.1:<rport> dans
// la session — pas de binaire `ssh`, pas de port local ni process.
// Sécurité : `Opts.expected_hk` épingle les empreintes SHA256 des host
// keys (profils appairés B′ — la divergence exige une re-confirmation
// explicite, jamais un accept silencieux) ; `Opts.key` porte la clé
// jetable K du pairing (mémoire, jamais disque).
// ---------------------------------------------------------------------------
use super::{base_dir, log, Level};
use anyhow::{bail, Context, Result};
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

/// Clé ed25519 générée par l'app : <base>/agentcontrol_ed25519{,.pub}
pub fn key_path() -> PathBuf {
    base_dir().join("agentcontrol_ed25519")
}

// rand 0.10 n'expose plus OsRng ; rand_core 0.10 non plus →
// petit RNG adossé à getrandom (CryptoRng vient par blanket impl).
struct OsRng;
impl rand_core::TryRng for OsRng {
    type Error = std::convert::Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut b = [0u8; 4];
        self.try_fill_bytes(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut b = [0u8; 8];
        self.try_fill_bytes(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn try_fill_bytes(&mut self, d: &mut [u8]) -> Result<(), Self::Error> {
        getrandom::fill(d).expect("os rng");
        Ok(())
    }
}
impl rand_core::TryCryptoRng for OsRng {}

/// Paire ed25519 en mémoire — clé jetable du pairing (jamais écrite
/// sur disque : elle vit dans le QR puis est scrubbée à l'usage).
pub fn gen_ed25519() -> Result<ssh_key::PrivateKey> {
    ssh_key::PrivateKey::random(&mut OsRng, ssh_key::Algorithm::Ed25519)
        .context("keygen ed25519")
}

/// (PEM privé, "<type> <b64>" public) d'une paire ed25519 jetable —
/// les deux formes nécessaires au pairing sans que l'appelant ait
/// besoin de la crate ssh_key.
pub fn gen_keypair_mem() -> Result<(String, String)> {
    let k = gen_ed25519()?;
    let pem = k
        .to_openssh(ssh_key::LineEnding::LF)
        .context("openssh pem")?
        .to_string();
    let pub_line = k.public_key().to_openssh().context("openssh pub")?;
    Ok((pem, pub_line))
}

/// Génère la paire si absente (chmod 600). Retourne le chemin.
pub fn ensure_keypair() -> Result<PathBuf> {
    let p = key_path();
    if p.exists() {
        return Ok(p);
    }
    let _ = std::fs::create_dir_all(base_dir());
    let key = gen_ed25519()?;
    let pem = key.to_openssh(ssh_key::LineEnding::LF).context("openssh pem")?;
    write_secret(&p, pem.as_bytes())?;
    let pub_line = format!("{} agentcontrol", key.public_key().to_openssh().unwrap_or_default());
    std::fs::write(p.with_extension("pub"), &pub_line)?;
    log(Level::Info, format!("clé ssh app générée : {}", pub_line));
    Ok(p)
}

/// Ligne authorized_keys à ajouter côté serveur ("ssh-ed25519 AAAA… agentcontrol").
pub fn pubkey_line() -> Result<String> {
    let p = ensure_keypair()?;
    let key = russh::keys::load_secret_key(&p, None).context("clé privée")?;
    Ok(format!(
        "{} agentcontrol",
        key.public_key().to_openssh().unwrap_or_default()
    ))
}

// ---------------------------------------------------------------------
// Appairage par QR : clé privée PEM échangée entre appareils (repli A)
// et clé jetable K (protocole B′).
// ---------------------------------------------------------------------

/// Valide une clé privée PEM OpenSSH — parse complet : un secret
/// tronqué ou malformé est refusé ici plutôt qu'au premier connect.
pub fn check_key_pem(pem: &str) -> Result<()> {
    russh::keys::decode_secret_key(pem, None)
        .context("clé privée illisible (format OpenSSH attendu)")?;
    Ok(())
}

/// Lit + valide un fichier de clé privée (le `~` est déjà expansé
/// par l'appelant). Erreur contextualisée si absent ou illisible.
pub fn read_key_pem(path: &Path) -> Result<String> {
    let pem = std::fs::read_to_string(path)
        .with_context(|| format!("clé {}", path.display()))?;
    check_key_pem(&pem)?;
    Ok(pem)
}

/// Clé privée reçue par appairage : écrite dans paired_keys/ (jamais
/// sur le chemin de la clé app-générée — celle-ci peut être déjà
/// autorisée ailleurs). Validée avant écriture, chmod 600.
/// `name` (ex. "hôte_port") est assaini en nom de fichier sûr ;
/// un nouvel appairage du même hôte écrase la clé précédente.
/// Retourne le chemin à poser dans cfg.key.
pub fn import_paired_key(pem: &str, name: &str) -> Result<PathBuf> {
    check_key_pem(pem)?;
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let dir = base_dir().join("paired_keys");
    std::fs::create_dir_all(&dir).context("paired_keys")?;
    let p = dir.join(if safe.is_empty() { "paired" } else { &safe });
    write_secret(&p, pem.as_bytes())
        .with_context(|| format!("écriture {}", p.display()))?;
    Ok(p)
}

/// Écrit une clé privée en 0600 dès la création — jamais de fenêtre
/// en permissions d'umask entre `write` et `chmod`.
fn write_secret(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    std::io::Write::write_all(&mut f, data)
}

/// Options d'une connexion native : pinning de la host key et/ou
/// clé privée décodée en mémoire.
#[derive(Clone, Default)]
pub struct Opts {
    /// Empreintes `SHA256:…` épinglées — la clé présentée par le
    /// serveur doit en faire partie (certificats refusés). `None` =
    /// accept-all historique (log seul). Un set vide est traité
    /// comme `None` : jamais produit par le pairing, et refuser tout
    /// serait un échec muet.
    pub expected_hk: Option<Arc<HashSet<String>>>,
    /// Clé privée en mémoire (K jetable du pairing — jamais écrite
    /// sur disque). Prioritaire sur `key_path` quand présente.
    pub key: Option<Arc<russh::keys::PrivateKey>>,
}

struct Handler {
    expected_hk: Option<Arc<HashSet<String>>>,
    /// Empreinte de la host key présentée au handshake — lue par
    /// connect_auth si le pinning a échoué (l'erreur russh ne fait
    /// pas remonter la clé vue). Partagée Arc<Mutex> : le Handler
    /// est consommé par client::connect.
    seen_fp: Arc<std::sync::Mutex<Option<String>>>,
}

impl client::Handler for Handler {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                let fp = key
                    .fingerprint(russh::keys::HashAlg::Sha256)
                    .to_string();
                if let Ok(mut g) = self.seen_fp.lock() {
                    *g = Some(fp.clone());
                }
                match &self.expected_hk {
                    Some(hk) => {
                        let ok = hk.contains(&fp);
                        log(
                            if ok { Level::Info } else { Level::Warn },
                            format!("ssh host key: {fp}{}", if ok { "" } else { " — REFUSÉE (hors pinning)" }),
                        );
                        Ok(ok)
                    }
                    // accept-all historique : la clé vue est loggée
                    // (TOFU implicite v1) — les profils appairés ont
                    // expected_hk et sortent de ce repli.
                    None => {
                        log(Level::Info, format!("ssh host key: {fp}"));
                        Ok(true)
                    }
                }
            }
            PublicKeyOrCertificate::Certificate(_) => {
                // Un profil épinglé n'attend que des host keys nues :
                // un certificat ne peut pas correspondre à `hk`.
                let pinned = self.expected_hk.is_some();
                log(
                    if pinned { Level::Warn } else { Level::Info },
                    format!("ssh host key: certificate{}", if pinned { " — refusé (pinning actif)" } else { "" }),
                );
                Ok(!pinned)
            }
        }
    }
}

/// Garde la session SSH vivante ; la fermeture suit le drop du Handle.
/// Le champ n'est jamais lu — c'est sa durée de vie qui compte.
pub struct NativeGuard(#[allow(dead_code)] client::Handle<Handler>);

/// Connecte + authentifie (clé, sinon mot de passe) et renvoie le handle.
async fn connect_auth(
    host: &str,
    port: u16,
    user: &str,
    password: Option<&str>,
    key: Option<&Path>,
    opts: &Opts,
) -> Result<client::Handle<Handler>> {
    let mut cfg = client::Config::default();
    cfg.inactivity_timeout = None;
    cfg.keepalive_interval = Some(Duration::from_secs(30));
    cfg.keepalive_max = 3;
    let addrs = (host, port);
    let seen_fp = Arc::new(std::sync::Mutex::new(None));
    let handler = Handler {
        expected_hk: opts.expected_hk.clone().filter(|s| !s.is_empty()),
        seen_fp: seen_fp.clone(),
    };
    let mut h = match tokio::time::timeout(
        Duration::from_secs(15),
        client::connect(Arc::new(cfg), addrs, handler),
    )
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            // Pinning actif + clé vue hors liste → erreur dédiée : le
            // frontend propose la re-confirmation explicite (TOFU),
            // jamais un accept silencieux.
            let fp = seen_fp.lock().ok().and_then(|g| g.clone());
            if let (Some(hk), Some(fp)) = (&opts.expected_hk, fp) {
                if !hk.is_empty() && !hk.contains(&fp) {
                    bail!("hostkey-mismatch:{fp}");
                }
            }
            return Err(e).context("ssh connect");
        }
        Err(_) => bail!("ssh connect timeout"),
    };
    // Clé en mémoire (pairing K) d'abord, puis fichier (app ou
    // explicite), puis mot de passe.
    if let Some(k) = &opts.key {
        let auth = h
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(k.clone(), None))
            .await;
        return match auth {
            Ok(r) if r.success() => Ok(h),
            _ => bail!("clé jetable refusée — QR expiré ou appairage annulé"),
        };
    }
    let kp = key.map(PathBuf::from).unwrap_or_else(key_path);
    if kp.exists() {
        let key = russh::keys::load_secret_key(&kp, None)
            .with_context(|| format!("clé {}", kp.display()))?;
        let auth = h
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
            .await;
        match auth {
            Ok(r) if r.success() => return Ok(h),
            Ok(_) => log(Level::Warn, "clé refusée (auth partielle) — essai mot de passe"),
            Err(e) => log(Level::Warn, format!("auth clé: {e} — essai mot de passe")),
        }
    }
    if let Some(pw) = password.filter(|p| !p.is_empty()) {
        let r = h
            .authenticate_password(user, pw)
            .await
            .context("auth password")?;
        if r.success() {
            return Ok(h);
        }
        bail!("mot de passe refusé");
    }
    bail!("auth impossible — ajoutez la clé publique de l'app dans authorized_keys du serveur");
}

/// `ssh host <script>` sans binaire : canal exec → stdout/stderr
/// collectés à part. `on_stderr` reçoit chaque ligne stderr complète
/// au fil de l'eau (progression `step:*`, erreurs lisibles…) — le
/// retour est `(code, stdout, stderr)` : le résultat métier reste
/// sur stdout, les messages de diagnostic sur stderr.
pub async fn ssh_exec(
    host: &str,
    port: u16,
    user: &str,
    password: Option<&str>,
    key_path: Option<&Path>,
    script: &str,
    on_stderr: Option<&mut (dyn FnMut(&str) + Send)>,
    opts: &Opts,
) -> Result<(i32, String, String)> {
    let h = connect_auth(host, port, user, password, key_path, opts).await?;
    exec_on(&h, script, on_stderr).await
}

/// Exec sur une session déjà ouverte — factorisé entre ssh_exec et
/// pair_attempt (connexion K jetable + pinning, ouverte par l'appelant).
async fn exec_on(
    h: &client::Handle<Handler>,
    script: &str,
    mut on_stderr: Option<&mut (dyn FnMut(&str) + Send)>,
) -> Result<(i32, String, String)> {
    use russh::ChannelMsg;
    let mut ch = h.channel_open_session().await.context("channel session")?;
    ch.exec(true, script.as_bytes().to_vec())
        .await
        .context("exec")?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    // Lignes stderr complètes → callback (les lignes partielles en
    // fin de flux sont livrées aussi, via le drain post-boucle).
    let mut err_rest = Vec::new();
    let mut feed_err = |data: &[u8]| {
        err.extend_from_slice(data);
        if let Some(cb) = on_stderr.as_deref_mut() {
            err_rest.extend_from_slice(data);
            while let Some(i) = err_rest.iter().position(|b| *b == b'\n') {
                let line = String::from_utf8_lossy(&err_rest[..i]).to_string();
                err_rest.drain(..=i);
                cb(line.trim_end_matches('\r'));
            }
        }
    };
    let mut code = -1i32;
    tokio::time::timeout(Duration::from_secs(120), async {
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => feed_err(&data),
                ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
                _ => {}
            }
        }
    })
    .await
    .context("exec timeout")?;
    // Reliquat stderr sans '\n' final → dernière ligne au callback.
    if !err_rest.is_empty() {
        if let Some(cb) = on_stderr.as_deref_mut() {
            cb(&String::from_utf8_lossy(&err_rest).to_string());
        }
    }
    Ok((
        code,
        String::from_utf8_lossy(&out).to_string(),
        String::from_utf8_lossy(&err).to_string(),
    ))
}

/// Ouvre un canal direct-tcpip vers 127.0.0.1:remote_port dans la
/// session SSH — équivalent `ssh -L` sans port local ni process.
pub async fn open_direct_tcpip(
    host: &str,
    port: u16,
    user: &str,
    remote_port: u16,
    password: Option<&str>,
    key_path: Option<&Path>,
    opts: &Opts,
) -> Result<(NativeGuard, impl AsyncRead + AsyncWrite + Unpin + Send)> {
    let h = connect_auth(host, port, user, password, key_path, opts).await?;
    let ch = tokio::time::timeout(
        Duration::from_secs(10),
        h.channel_open_direct_tcpip("127.0.0.1", remote_port as u32, "127.0.0.1", 0),
    )
    .await
    .context("direct-tcpip timeout")?
    .context("direct-tcpip — le démon écoute-t-il sur le port distant ?")?;
    Ok((NativeGuard(h), ch.into_stream()))
}

/// Appairage B′ — côté appareil qui scanne le QR : ouvre une session
/// authentifiée par la clé jetable K (PEM, mémoire seulement) en
/// épinglant `hk` (MITM tué au handshake), exécute la forced command
/// `agentcontrol-pair <type> <b64> <label>` et exige un retour
/// `pair-ok:<fp>` dont l'empreinte est celle de NOTRE clé app —
/// preuve que c'est bien elle qui vient d'être installée, pas un
/// ack vide forgé. Retourne le fingerprint confirmé.
pub async fn pair_attempt(
    host: &str,
    port: u16,
    user: &str,
    k_pem: &str,
    hk: HashSet<String>,
    label: &str,
) -> Result<String> {
    let kp = ensure_keypair()?;
    let own = russh::keys::load_secret_key(&kp, None).context("clé app")?;
    let own_pub = own.public_key().to_openssh().unwrap_or_default();
    let own_fp = own
        .public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string();
    let mut it = own_pub.split_whitespace();
    let (ty, b64) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    if ty.is_empty() || b64.is_empty() {
        bail!("pubkey app illisible");
    }
    let key = russh::keys::decode_secret_key(k_pem, None)
        .context("clé jetable K illisible — QR incomplet ?")?;
    let opts = Opts {
        expected_hk: Some(Arc::new(hk)),
        key: Some(Arc::new(key)),
    };
    let h = connect_auth(host, port, user, None, None, &opts)
        .await
        .context("connexion K — QR expiré ou hôte injoignable")?;
    let cmd = format!("agentcontrol-pair {ty} {b64} {label}");
    let (code, out, err) = exec_on(&h, &cmd, None).await?;
    let fp = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("pair-ok:"))
        .map(str::trim)
        .with_context(|| {
            format!(
                "pair.sh a refusé (code {code}){}",
                if err.trim().is_empty() {
                    String::new()
                } else {
                    format!(" : {}", err.trim())
                }
            )
        })?;
    if fp != own_fp {
        bail!("pair-ok incohérent — la clé installée n'est pas la nôtre");
    }
    Ok(fp.to_string())
}
