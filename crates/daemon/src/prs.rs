//! Registre PR ↔ session : quel agent a créé quelle pull request /
//! merge request / change Gerrit.
//!
//! Persisté en JSON (`<root>/prs.json`) — le démon est l'unique writer,
//! un BTreeMap + rewrite atomique suffit (pas de concurrents directs).
//! Alimenté par :
//!  - la détection automatique : un watcher par session scanne les
//!    extraits des tool calls à la recherche d'URLs de review
//!    (`github.com/<o>/<r>/pull/<N>`, GitLab `/-/merge_requests/<N>`,
//!    Gerrit `/c|/r/<p>/+/<N>`, `Change-Id: I<sha>`) — voir `watch_prs`
//!    dans server/prs.rs et le parsing par forge dans `forges.rs` ;
//!  - les upserts manuels (`acctl pr add` → ReqKind::PrTrack), qui
//!    priment toujours sur `via: "detected"`.
//!
//! Identité d'une entrée : `forge|host|repo#N` — ou `forge|host|#I<sha>`
//! pour un change Gerrit connu par son seul Change-Id (pr=0) ; quand le
//! numéro est résolu (sync), l'entrée est re-clée sur le numéro et garde
//! le Change-Id (les deux formes de détection la retrouvent alors).

use anyhow::{Context, Result};
use proto::PrEntry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::forges::{Forges, PrRef};

pub struct PrRegistry {
    path: PathBuf,
    map: Mutex<BTreeMap<String, PrEntry>>,
}

/// Paramètres d'un upsert manuel (`pr_track`).
pub struct TrackIn {
    pub session: Option<String>,
    pub repo: String,
    pub pr: u64,
    pub forge: String,
    pub host: String,
    pub change_id: Option<String>,
    pub url: Option<String>,
    pub branch: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    pub note: Option<String>,
    /// Compteur d'échecs de sync écrit tel quel (`pr sync` d'acctl) —
    /// None conserve, Some(0) = l'entrée a été lue live.
    pub sync_fails: Option<u32>,
}

impl PrRegistry {
    /// Charge `<root>/prs.json` — un fichier absent ou illisible démarre
    /// un registre vide (le démon ne doit pas mourir pour ça). Les
    /// entrées pré-multi-forges (sans forge/host/url) sont normalisées
    /// en github/github.com à la charge.
    pub fn open(root: &Path) -> Self {
        let path = root.join("prs.json");
        let mut map = BTreeMap::new();
        if path.exists() {
            match std::fs::read_to_string(&path)
                .context("prs.json")
                .and_then(|s| Ok(serde_json::from_str::<serde_json::Value>(&s)?))
            {
                Ok(v) => {
                    for e in v
                        .get("prs")
                        .and_then(|a| a.as_array())
                        .map(|a| a.as_slice())
                        .unwrap_or(&[])
                    {
                        if let Ok(mut e) = serde_json::from_value::<PrEntry>(e.clone()) {
                            normalize(&mut e);
                            map.insert(entry_key(&e), e);
                        }
                    }
                }
                Err(e) => tracing::warn!("prs.json ignoré ({e:#}) — registre vide"),
            }
        }
        Self {
            path,
            map: Mutex::new(map),
        }
    }

    pub fn list(&self) -> Vec<PrEntry> {
        // BTreeMap trié par clé canonique — présenté du plus récent au plus ancien.
        self.map.lock().unwrap().values().rev().cloned().collect()
    }

    /// Upsert manuel (`pr_track`) : `Some` écrase, `None` conserve ;
    /// `via: "manual"` (jamais rétrogradé par la détection) uniquement
    /// quand `session` est fourni — une retouche de métadonnées (note,
    /// sync) ne doit pas sanctifier une attribution auto.
    ///
    /// Résolution de la cible : une entrée portant `change_id` (identité
    /// forte) prime sur la clé numérique. Si l'upsert résout un
    /// Change-Id (pr 0 → N), l'entrée est re-clée sur le numéro ; la clé
    /// numérique déjà prise → les champs fusionnent dans l'entrée
    /// numérique et la fiche Change-Id disparaît.
    pub fn track(&self, t: TrackIn) -> Result<PrEntry> {
        let forge = proto::forge_kind(&t.forge).to_string();
        anyhow::ensure!(
            matches!(forge.as_str(), "github" | "gitlab" | "gerrit"),
            "forge inconnue: {forge}"
        );
        let host = if t.host.is_empty() {
            proto::forge_default_host(&forge).to_string()
        } else {
            t.host.to_lowercase()
        };
        anyhow::ensure!(!host.is_empty(), "hôte requis pour la forge {forge}");
        anyhow::ensure!(
            t.pr > 0 || t.change_id.is_some(),
            "repo/pr invalides — ou Change-Id requis"
        );
        anyhow::ensure!(
            !t.repo.is_empty() || t.change_id.is_some(),
            "repo vide — Change-Id requis"
        );

        let mut g = self.map.lock().unwrap();
        // Priorité à la fiche portant le Change-Id (présente ou non sous
        // une clé numérique), puis à la clé numérique.
        let cid_hit = t
            .change_id
            .as_deref()
            .and_then(|c| find_by_cid(&g, &forge, &host, c));
        let num_hit = (t.pr > 0 && !t.repo.is_empty())
            .then(|| num_key(&forge, &host, &t.repo, t.pr))
            .filter(|k| g.contains_key(k));
        let found = cid_hit.or(num_hit);

        let e = match found {
            Some(k) => {
                let mut e = g.remove(&k).expect("clé résolue absente");
                apply_track(&mut e, &t, &forge, &host);
                // Re-clé si l'identité canonique a changé (Change-Id →
                // numéro résolu). Clé cible déjà prise → fusion : les
                // mêmes updates sont appliqués à l'entrée numérique, qui
                // récupère ce que la fiche cid apporte (la fiche cid
                // porte souvent la vraie attribution — détection issue
                // du `git review` réel), puis elle est absorbée.
                let nk = entry_key(&e);
                if nk != k && g.contains_key(&nk) {
                    let mut other = g.remove(&nk).expect("clé cible absente");
                    apply_track(&mut other, &t, &forge, &host);
                    if other.change_id.is_none() {
                        other.change_id = e.change_id.clone();
                    }
                    if other.note.is_none() {
                        other.note = e.note.clone();
                    }
                    if other.title.is_none() {
                        other.title = e.title.clone();
                    }
                    if other.branch.is_none() {
                        other.branch = e.branch.clone();
                    }
                    // Pas de session explicite et l'entrée numérique
                    // n'est qu'une détection → la fiche cid (vrai git
                    // review) a meilleure attribution.
                    if t.session.is_none() && other.via == "detected" {
                        other.session = e.session.clone();
                        other.via = e.via.clone();
                    }
                    if e.created_at < other.created_at {
                        other.created_at = e.created_at.clone();
                    }
                    g.insert(nk, other.clone());
                    other
                } else {
                    g.insert(nk, e.clone());
                    e
                }
            }
            None => {
                let session =
                    t.session.clone().context("session requise pour une nouvelle entrée")?;
                let tstamp = proto::now_iso();
                let url = t
                    .url
                    .clone()
                    .unwrap_or_else(|| {
                        proto::forge_url(
                            &forge,
                            &host,
                            &t.repo,
                            t.pr,
                            t.change_id.as_deref(),
                        )
                    });
                let e = PrEntry {
                    repo: t.repo.clone(),
                    pr: t.pr,
                    forge,
                    host,
                    url,
                    change_id: t.change_id.clone(),
                    session,
                    branch: t.branch.clone(),
                    title: t.title.clone(),
                    status: t.status.clone().unwrap_or_else(|| "open".into()),
                    note: t.note.clone(),
                    via: "manual".into(),
                    sync_fails: t.sync_fails.unwrap_or(0),
                    created_at: tstamp.clone(),
                    updated_at: tstamp,
                };
                let k = entry_key(&e);
                g.insert(k, e.clone());
                e
            }
        };
        drop(g);
        self.save()?;
        Ok(e)
    }

    /// Détection auto (watcher) : n'écrase JAMAIS une entrée manuelle ;
    /// n'écrase une détection existante que si `strong` (le tool call
    /// ressemblait à une création — `gh pr create`, `glab mr create`,
    /// `git review`, push `refs/for/*`) et que la session diffère —
    /// sinon no-op (évite de ré-émettre/bumper à chaque rescan de
    /// journal au boot). Retourne true quand l'entrée a été
    /// créée/réattribuée.
    pub fn detected(&self, session: &str, r: &PrRef, strong: bool) -> Result<bool> {
        let mut g = self.map.lock().unwrap();
        // Change-Id : l'entrée peut être clée numériquement (déjà
        // résolue) — chercher par le cid, pas seulement la clé cid.
        let k = r
            .change_id
            .as_deref()
            .and_then(|c| find_by_cid(&g, r.forge, &r.host, c))
            .unwrap_or_else(|| ref_key(r));
        match g.get_mut(&k) {
            Some(old)
                if old.via == "manual" || !strong || old.session == session =>
            {
                return Ok(false);
            }
            Some(old) => {
                old.session = session.into();
                old.updated_at = proto::now_iso();
            }
            None => {
                let t = proto::now_iso();
                g.insert(
                    k,
                    PrEntry {
                        repo: r.repo.clone(),
                        pr: r.pr,
                        forge: r.forge.into(),
                        host: r.host.clone(),
                        url: r.url.clone(),
                        change_id: r.change_id.clone(),
                        session: session.into(),
                        branch: None,
                        title: None,
                        status: "open".into(),
                        note: None,
                        via: "detected".into(),
                        sync_fails: 0,
                        created_at: t.clone(),
                        updated_at: t,
                    },
                );
            }
        }
        drop(g);
        self.save()?;
        Ok(true)
    }

    /// Statut vu dans une sortie outil (ligne « Merged pull request
    /// o/r#N », « Merged merge request », URL + « merged » pour Gerrit) :
    /// fait objectif, pas une attribution — met à jour `status` quelle
    /// que soit la session propriétaire ou `via`. No-op si l'entrée est
    /// absente ou déjà à ce statut. Retourne true si changé.
    pub fn state_seen(&self, r: &PrRef, status: &str) -> Result<bool> {
        let mut g = self.map.lock().unwrap();
        let k = r
            .change_id
            .as_deref()
            .and_then(|c| find_by_cid(&g, r.forge, &r.host, c))
            .unwrap_or_else(|| ref_key(r));
        let Some(old) = g.get_mut(&k) else {
            return Ok(false);
        };
        if old.status == status {
            return Ok(false);
        }
        old.status = status.into();
        old.updated_at = proto::now_iso();
        drop(g);
        self.save()?;
        Ok(true)
    }

    /// Retire une entrée — adressage identique à `track` (Change-Id
    /// prioritaire, sinon clé numérique).
    pub fn remove(&self, forge: &str, host: &str, repo: &str, pr: u64, change_id: Option<&str>) -> Result<bool> {
        let mut g = self.map.lock().unwrap();
        let k = entry_lookup_key(&g, forge, host, repo, pr, change_id);
        let gone = g.remove(&k).is_some();
        drop(g);
        if gone {
            self.save()?;
        }
        Ok(gone)
    }

    /// Verdict d'un passage de sync sur une entrée : `ok` = la forge a
    /// répondu → compteur d'échecs remis à zéro ; `!ok` = injoignable
    /// (hôte mort, repo absent, référence inconnue) → incrémenté.
    /// Passé `proto::PR_UNRESOLVED_AFTER` le client l'affiche « non
    /// résoluble ». No-op sur une entrée absente ou un compteur déjà
    /// à la bonne valeur.
    pub fn sync_seen(
        &self,
        forge: &str,
        host: &str,
        repo: &str,
        pr: u64,
        change_id: Option<&str>,
        ok: bool,
    ) -> Result<bool> {
        let mut g = self.map.lock().unwrap();
        let k = entry_lookup_key(&g, forge, host, repo, pr, change_id);
        let Some(e) = g.get_mut(&k) else {
            return Ok(false);
        };
        let nf = if ok { 0 } else { e.sync_fails.saturating_add(1) };
        if e.sync_fails == nf {
            return Ok(false);
        }
        e.sync_fails = nf;
        e.updated_at = proto::now_iso();
        drop(g);
        self.save()?;
        Ok(true)
    }

    /// Rewrite atomique (tmp + rename) — un kill en cours d'écriture ne
    /// laisse pas un fichier tronqué.
    fn save(&self) -> Result<()> {
        let v = {
            let g = self.map.lock().unwrap();
            serde_json::json!({ "prs": g.values().collect::<Vec<_>>() })
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Normalise une entrée chargée : forge "" → github, host "" → hôte par
/// défaut de la forge, url "" → canonique.
fn normalize(e: &mut PrEntry) {
    e.forge = proto::forge_kind(&e.forge).to_string();
    if e.host.is_empty() {
        e.host = proto::forge_default_host(&e.forge).to_string();
    }
    if e.url.is_empty() {
        e.url = proto::forge_url(&e.forge, &e.host, &e.repo, e.pr, e.change_id.as_deref());
    }
}

/// Applique les champs `Some` de `t` sur `e` — partagé entre la branche
/// trouvée de `track` et la fusion de clés.
fn apply_track(e: &mut PrEntry, t: &TrackIn, forge: &str, host: &str) {
    if let Some(s) = t.session.clone() {
        e.session = s;
        e.via = "manual".into();
    }
    e.forge = forge.into();
    e.host = host.into();
    if !t.repo.is_empty() {
        e.repo = t.repo.clone();
    }
    if t.pr > 0 {
        e.pr = t.pr;
    }
    if let Some(c) = t.change_id.clone() {
        e.change_id = Some(c);
    }
    e.url = t
        .url
        .clone()
        .unwrap_or_else(|| {
            proto::forge_url(&e.forge, &e.host, &e.repo, e.pr, e.change_id.as_deref())
        });
    if let Some(v) = t.branch.clone() {
        e.branch = Some(v);
    }
    if let Some(v) = t.title.clone() {
        e.title = Some(v);
    }
    if let Some(v) = t.status.clone() {
        e.status = v;
    }
    if let Some(v) = t.note.clone() {
        e.note = Some(v);
    }
    if let Some(v) = t.sync_fails {
        e.sync_fails = v;
    }
    e.updated_at = proto::now_iso();
}

/// Clé de recherche d'une entrée à partir d'un adressage
/// `forge/host/repo/pr/change_id` — Change-Id prioritaire (la fiche
/// peut être clée cid ou numérique), sinon clé numérique. Partagé par
/// `remove` et `sync_seen`. Un host vide prend le défaut de la forge.
fn entry_lookup_key(
    g: &BTreeMap<String, PrEntry>,
    forge: &str,
    host: &str,
    repo: &str,
    pr: u64,
    change_id: Option<&str>,
) -> String {
    let forge = proto::forge_kind(forge);
    let host = if host.is_empty() {
        proto::forge_default_host(forge)
    } else {
        host
    };
    match change_id.and_then(|c| find_by_cid(g, forge, host, c)) {
        Some(k) => k,
        None if pr == 0 => match change_id {
            Some(c) => cid_key(forge, host, c),
            None => num_key(forge, host, repo, pr),
        },
        None => num_key(forge, host, repo, pr),
    }
}

/// Clé canonique numérique — insensible à la casse sur forge/host/repo
/// (github ET gitlab le sont sur leurs paths).
fn num_key(forge: &str, host: &str, repo: &str, pr: u64) -> String {
    format!(
        "{}|{}|{}#{}",
        proto::forge_kind(forge),
        host.to_lowercase(),
        repo.to_lowercase(),
        pr
    )
}

/// Clé d'un change Gerrit non résolu — le Change-Id seul identifie.
fn cid_key(forge: &str, host: &str, change_id: &str) -> String {
    format!(
        "{}|{}|#{}",
        proto::forge_kind(forge),
        host.to_lowercase(),
        change_id.to_lowercase()
    )
}

/// Clé d'une référence détectée — même règle que `entry_key`.
fn ref_key(r: &PrRef) -> String {
    if r.pr == 0 {
        if let Some(cid) = r.change_id.as_deref() {
            return cid_key(r.forge, &r.host, cid);
        }
    }
    num_key(r.forge, &r.host, &r.repo, r.pr)
}

/// Clé d'une entrée : Change-Id tant que `pr` est 0, sinon numérique.
fn entry_key(e: &PrEntry) -> String {
    if let Some(cid) = e.change_id.as_deref().filter(|_| e.pr == 0) {
        cid_key(&e.forge, &e.host, cid)
    } else {
        num_key(&e.forge, &e.host, &e.repo, e.pr)
    }
}

/// Retrouve la clé de l'entrée portant `change_id` sur (forge, host) —
/// la fiche peut être clée cid (non résolue) OU numérique (résolue, le
/// cid conservé). Insensible à la casse (Change-Ids hexadécimaux).
fn find_by_cid(
    g: &BTreeMap<String, PrEntry>,
    forge: &str,
    host: &str,
    cid: &str,
) -> Option<String> {
    g.iter()
        .find(|(_, e)| {
            e.forge == forge
                && e.host.eq_ignore_ascii_case(host)
                && e.change_id
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case(cid))
        })
        .map(|(k, _)| k.clone())
}

/// Extrait les `github.com/<owner>/<repo>/pull/<N>` d'un texte — forme
/// historique, conservée pour `extract_state_refs` (tokens `o/r#N`).
fn extract_repo_num_refs(text: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| {
        !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '#'))
    }) {
        let Some((repo, num)) = tok.split_once('#') else {
            continue;
        };
        if repo.matches('/').count() != 1 || repo.starts_with('/') || repo.ends_with('/') {
            continue;
        }
        // Tokens de doc (`o/r#N`, `owner/repo#7`) — même denylist que
        // la détection d'URLs : pas de mise à jour d'état fantôme.
        if crate::forges::placeholder_repo(repo) {
            continue;
        }
        let digits: String = num.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u64>() {
            if n > 0 && !out.iter().any(|(r, p)| r == repo && *p == n) {
                out.push((repo.to_string(), n));
            }
        }
    }
    out
}

/// Extrait les fins de review d'une sortie outil, toutes forges :
/// formulations d'action gh (« Merged pull request o/r#N ») → refs
/// github (tokens + URLs) ; glab (« Merged merge request ») → refs
/// gitlab de la ligne (URL seulement — un `x/y!N` nu ne dit pas l'hôte) ;
/// Gerrit : « merged »/« abandoned »/« restored » sur une ligne portant
/// une URL gerrit. Gardes :
///  - lignes JSON (`{`/`[` en tête) ignorées : `gh pr view --json`
///    contient `mergedAt`/`"merged":false` sans être un merge ;
///  - « will be » / « queued » = auto-merge différé, pas un fait ;
///  - négations (« was not merged ») ignorées.
/// Dernier statut vu gagne par référence dans le texte.
pub fn extract_state_refs(text: &str, forges: &Forges) -> Vec<(PrRef, &'static str)> {
    let mut out: Vec<(PrRef, &'static str)> = Vec::new();
    for line in text.lines() {
        let l = line.to_lowercase();
        if l.trim_start().starts_with('{') || l.trim_start().starts_with('[') {
            continue; // sortie JSON mono-ligne (gh --json, gh api)
        }
        if l.contains("will be") || l.contains("queued") {
            continue;
        }
        if l.contains("not merged") || l.contains("mergeable") {
            continue;
        }
        // Statuts par famille de forge — les URLs détectées dans la
        // ligne portent l'hôte ; les tokens `o/r#N` restent github.
        let status = if l.contains("merged pull request") {
            Some(("merged", "pr"))
        } else if l.contains("closed pull request") {
            Some(("closed", "pr"))
        } else if l.contains("reopened pull request") {
            Some(("open", "pr"))
        } else if l.contains("merged merge request")
            || l.contains("merged merge_request")
        {
            Some(("merged", "mr"))
        } else if l.contains("closed merge request")
            || l.contains("closed merge_request")
        {
            Some(("closed", "mr"))
        } else if l.contains("reopened merge request")
            || l.contains("reopened merge_request")
        {
            Some(("open", "mr"))
        } else if l.contains(" merged") || l.contains("merged ") {
            Some(("merged", "gerrit"))
        } else if l.contains("abandoned") {
            Some(("closed", "gerrit"))
        } else if l.contains("restored") {
            Some(("open", "gerrit"))
        } else {
            None
        };
        let Some((status, family)) = status else { continue };
        // Références candidates de la ligne : tokens `o/r#N` → github
        // uniquement ; URLs → la forge correspondante si éligible.
        let mut refs: Vec<PrRef> = if family == "pr" {
            extract_repo_num_refs(line)
                .into_iter()
                .map(|(repo, n)| PrRef {
                    forge: "github",
                    host: "github.com".into(),
                    url: proto::forge_url("github", "github.com", &repo, n, None),
                    repo,
                    pr: n,
                    change_id: None,
                })
                .collect()
        } else {
            Vec::new()
        };
        for r in forges.extract_refs(line) {
            let ok = match family {
                "pr" => r.forge == "github",
                "mr" => r.forge == "gitlab",
                "gerrit" => r.forge == "gerrit",
                _ => false,
            };
            if ok {
                refs.push(r);
            }
        }
        for r in refs {
            match out.iter_mut().find(|(x, _)| {
                x.forge == r.forge && x.host == r.host && x.pr == r.pr
                    && x.repo.eq_ignore_ascii_case(&r.repo)
                    && x.change_id == r.change_id
            }) {
                Some(e) => e.1 = status, // dernier vu gagne
                None => out.push((r, status)),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{extract_state_refs, Forges, PrRef, PrRegistry, TrackIn};

    /// Registre sur un root temporaire (séquencé par pid+nanos).
    fn reg() -> PrRegistry {
        let d = std::env::temp_dir().join(format!(
            "ac-prs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        PrRegistry::open(&d)
    }

    fn gl_ref(repo: &str, pr: u64) -> PrRef {
        PrRef {
            forge: "gitlab",
            host: "gitlab.corp".into(),
            repo: repo.into(),
            pr,
            change_id: None,
            url: proto::forge_url("gitlab", "gitlab.corp", repo, pr, None),
        }
    }

    fn ger_ref(repo: &str, pr: u64, cid: Option<&str>) -> PrRef {
        PrRef {
            forge: "gerrit",
            host: "gerrit.corp:8080".into(),
            repo: repo.into(),
            pr,
            change_id: cid.map(String::from),
            url: proto::forge_url("gerrit", "gerrit.corp:8080", repo, pr, cid),
        }
    }

    #[test]
    fn attribution_multi_forges() {
        let r = reg();
        // Une détection par forge — les clés ne se marchent pas dessus.
        r.detected("s-1", &gl_ref("g/p", 5), true).unwrap();
        r.detected("s-1", &ger_ref("proj", 9, None), true).unwrap();
        let v = r.list();
        assert_eq!(v.len(), 2);
        let gl = v.iter().find(|e| e.forge == "gitlab").unwrap();
        assert_eq!((gl.repo.as_str(), gl.pr), ("g/p", 5));
        let ge = v.iter().find(|e| e.forge == "gerrit").unwrap();
        assert_eq!(ge.host, "gerrit.corp:8080");

        // Détection faible d'une autre session → pas de réattribution.
        r.detected("s-2", &gl_ref("g/p", 5), false).unwrap();
        assert_eq!(r.list()[..].iter().find(|e| e.forge == "gitlab").unwrap().session, "s-1");
        // Forte → réattribution (glab mr create d'une autre session).
        r.detected("s-2", &gl_ref("g/p", 5), true).unwrap();
        assert_eq!(r.list()[..].iter().find(|e| e.forge == "gitlab").unwrap().session, "s-2");
        // Manual prime sur la détection forte.
        r.track(TrackIn {
            session: Some("s-9".into()),
            repo: "g/p".into(),
            pr: 5,
            forge: "gitlab".into(),
            host: "gitlab.corp".into(),
            change_id: None,
            url: None,
            branch: None,
            title: None,
            status: None,
            note: None,
            sync_fails: None,
        })
        .unwrap();
        r.detected("s-3", &gl_ref("g/p", 5), true).unwrap();
        assert_eq!(r.list()[..].iter().find(|e| e.forge == "gitlab").unwrap().session, "s-9");
    }

    #[test]
    fn gerrit_cid_rekey() {
        let r = reg();
        let cid = "Ideadbeefcafe0123456789abcdef0123456789ab";
        // Détecté par Change-Id seul → clé cid, pr=0, badge cid.
        r.detected("s-1", &ger_ref("", 0, Some(cid)), true).unwrap();
        let v = r.list();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].pr, 0);
        assert_eq!(v[0].change_id.as_deref(), Some(cid));

        // Le sync résout (numéro + projet) → re-clé numérique, cid gardé.
        r.track(TrackIn {
            session: None,
            repo: "proj".into(),
            pr: 7,
            forge: "gerrit".into(),
            host: "gerrit.corp:8080".into(),
            change_id: Some(cid.into()),
            url: None,
            branch: None,
            title: None,
            status: Some("merged".into()),
            note: None,
            sync_fails: None,
        })
        .unwrap();
        let v = r.list();
        assert_eq!(v.len(), 1, "le cid doit fusionner, pas dupliquer");
        assert_eq!((v[0].repo.as_str(), v[0].pr), ("proj", 7));
        assert_eq!(v[0].status, "merged");
        assert_eq!(v[0].change_id.as_deref(), Some(cid));

        // La même URL numérique retrouve l'entrée (et vice-versa le cid).
        r.detected("s-2", &ger_ref("proj", 7, None), true).unwrap();
        assert_eq!(r.list()[..][0].session, "s-2");
        // Suppression adressée par le Change-Id.
        assert!(r.remove("gerrit", "gerrit.corp:8080", "", 0, Some(cid)).unwrap());
        assert!(r.list().is_empty());
    }

    #[test]
    fn remove_multi_forges() {
        let r = reg();
        r.detected("s-1", &gl_ref("grp/sub/proj", 42), true).unwrap();
        r.detected("s-1", &gh("acme/widgets", 42), true).unwrap();
        // Clé complète forge+host+repo+numéro : la MR GitLab part sans
        // toucher la PR github de même numéro.
        assert!(r
            .remove("gitlab", "gitlab.corp", "grp/sub/proj", 42, None)
            .unwrap());
        let v = r.list();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].forge, "github");
        // Seconde fois : déjà partie → removed:false.
        assert!(!r
            .remove("gitlab", "gitlab.corp", "grp/sub/proj", 42, None)
            .unwrap());
        // Host omis → défaut de la forge (« gitlab.com » ≠ gitlab.corp).
        assert!(!r.remove("gitlab", "", "grp/sub/proj", 42, None).unwrap());
        assert!(r.remove("github", "", "acme/widgets", 42, None).unwrap());
        assert!(r.list().is_empty());
    }

    #[test]
    fn sync_fails_counting() {
        let r = reg();
        r.detected("s-1", &gh("acme/widgets", 7), true).unwrap();
        // Trois syncs sans lecture live → « non résoluble ».
        for i in 1..=3 {
            assert!(r.sync_seen("github", "", "acme/widgets", 7, None, false).unwrap());
            assert_eq!(r.list()[0].sync_fails, i);
        }
        // Quatrième échec : le compteur continue (saturé u32).
        r.sync_seen("github", "", "acme/widgets", 7, None, false).unwrap();
        assert!(r.list()[0].sync_fails >= proto::PR_UNRESOLVED_AFTER);
        // Lecture live → remise à zéro ; un second ok est un no-op.
        assert!(r.sync_seen("github", "", "acme/widgets", 7, None, true).unwrap());
        assert_eq!(r.list()[0].sync_fails, 0);
        assert!(!r.sync_seen("github", "", "acme/widgets", 7, None, true).unwrap());
        // Entrée absente → no-op.
        assert!(!r.sync_seen("github", "", "acme/widgets", 9, None, false).unwrap());
    }

    fn frefs() -> Forges {
        Forges::resolve(&[
            crate::forges::ForgeCfg {
                kind: "gitlab".into(),
                hosts: vec![],
                token_env: None,
            },
            crate::forges::ForgeCfg {
                kind: "gerrit".into(),
                hosts: vec!["gerrit.corp".into()],
                token_env: None,
            },
        ])
    }

    fn gh(repo: &str, pr: u64) -> PrRef {
        PrRef {
            forge: "github",
            host: "github.com".into(),
            repo: repo.into(),
            pr,
            change_id: None,
            url: proto::forge_url("github", "github.com", repo, pr, None),
        }
    }

    #[test]
    fn states_github() {
        let f = frefs();
        assert_eq!(
            extract_state_refs("✓ Merged pull request Wameuh/AgentControl#65", &f),
            vec![(gh("Wameuh/AgentControl", 65), "merged")]
        );
        assert_eq!(
            extract_state_refs("✓ Closed pull request acme/widgets#7.", &f),
            vec![(gh("acme/widgets", 7), "closed")]
        );
        assert_eq!(
            extract_state_refs("✓ Reopened pull request acme/widgets#2", &f),
            vec![(gh("acme/widgets", 2), "open")]
        );
        // Statut via l'URL quand gh l'affiche plutôt que o/r#N.
        assert_eq!(
            extract_state_refs("Merged pull request https://github.com/acme/widgets/pull/9", &f),
            vec![(gh("acme/widgets", 9), "merged")]
        );
        // Placeholders de doc ignorés — pas d'état fantôme.
        assert!(extract_state_refs("✓ Merged pull request o/r#9", &f).is_empty());
        assert!(extract_state_refs(
            "Merged pull request https://github.com/owner/repo/pull/9",
            &f
        )
        .is_empty());
        // « merged » sans la forme d'action gh + URL github : la forme
        // générique « merged » ne s'applique qu'aux refs gerrit.
        assert!(extract_state_refs("merged https://github.com/acme/widgets/pull/9", &f).is_empty());
        // Auto-merge différé : la PR reste ouverte.
        assert!(extract_state_refs(
            "✓ Pull request acme/widgets#9 will be merged when all requirements are met", &f
        )
        .is_empty());
        // Titre de merge-commit git ≠ merge effectué (« Merge », pas « Merged »).
        assert!(extract_state_refs("Merge pull request #65 from acme/widgets", &f).is_empty());
        // « not mergeable » ne contient pas « merged pull request ».
        assert!(extract_state_refs("X Pull request acme/widgets#5 is not mergeable", &f).is_empty());
        // Négations.
        assert!(extract_state_refs("acme/widgets#5 was not merged", &f).is_empty());
        // JSON mono-ligne ignoré.
        assert!(extract_state_refs(
            r#"{"state":"OPEN","mergedAt":null,"closedAt":null,"url":"https://github.com/acme/widgets/pull/5"}"#,
            &f
        )
        .is_empty());
        // Dernier statut vu gagne.
        assert_eq!(
            extract_state_refs(
                "✓ Reopened pull request acme/widgets#5\n✓ Merged pull request acme/widgets#5",
                &f
            ),
            vec![(gh("acme/widgets", 5), "merged")]
        );
        assert!(extract_state_refs("gh pr view https://github.com/acme/widgets/pull/3", &f).is_empty());
    }

    #[test]
    fn states_gitlab() {
        let f = frefs();
        let mr = PrRef {
            forge: "gitlab",
            host: "gitlab.com".into(),
            repo: "corp/tools".into(),
            pr: 42,
            change_id: None,
            url: "https://gitlab.com/corp/tools/-/merge_requests/42".into(),
        };
        assert_eq!(
            extract_state_refs(
                "Merged merge request https://gitlab.com/corp/tools/-/merge_requests/42",
                &f
            ),
            vec![(mr.clone(), "merged")]
        );
        assert_eq!(
            extract_state_refs(
                "Closed merge request https://gitlab.com/corp/tools/-/merge_requests/42",
                &f
            ),
            vec![(mr.clone(), "closed")]
        );
        // « merge request » ne touche pas une URL github de la ligne.
        assert!(extract_state_refs(
            "Merged merge request https://github.com/acme/widgets/pull/9",
            &f
        )
        .is_empty());
        // Une URL gitlab dans une ligne « pull request » n'est pas prise.
        assert!(extract_state_refs(
            "Merged pull request https://gitlab.com/corp/tools/-/merge_requests/42",
            &f
        )
        .is_empty());
    }

    #[test]
    fn states_gerrit() {
        let f = frefs();
        let ch = PrRef {
            forge: "gerrit",
            host: "gerrit.corp".into(),
            repo: "core/app".into(),
            pr: 9,
            change_id: None,
            url: "https://gerrit.corp/c/core/app/+/9".into(),
        };
        assert_eq!(
            extract_state_refs("remote: Change https://gerrit.corp/c/core/app/+/9 merged", &f),
            vec![(ch.clone(), "merged")]
        );
        assert_eq!(
            extract_state_refs("https://gerrit.corp/c/core/app/+/9 abandoned", &f),
            vec![(ch.clone(), "closed")]
        );
        assert_eq!(
            extract_state_refs("https://gerrit.corp/c/core/app/+/9 restored", &f),
            vec![(ch, "open")]
        );
        // « was not merged » gardé.
        assert!(extract_state_refs("https://gerrit.corp/c/core/app/+/9 was not merged", &f)
            .is_empty());
    }
}
