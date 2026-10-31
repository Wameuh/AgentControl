#!/bin/sh
# ac-pair-version: 4
# ~/.agentcontrol/pair.sh <nonce> <rport>
#
# Forced command posée par authorized_keys via une ligne :
#   restrict[,expiry-time=…],command="/abs/pair.sh <nonce> <rport>" \
#     ssh-ed25519 <K.pub> agentcontrol-pair-tmp <nonce>
# Le nonce est écrit dans la ligne par l'install — le client ne fournit
# RIEN : SSH_ORIGINAL_COMMAND n'est que la demande métier
#   agentcontrol-pair <type> <b64> <label>   → installe la clé (one-shot)
#   agentcontrol-pair-revoke               → annulation pendant la fenêtre
#
# Appel direct hors SSH : `pair.sh --sweep` = ménage des lignes K et des
# états expirés (cron utilisateur ou hook d'install).
#
# Durcissements (audit B′) : PATH/IFS/umask fixés, aucun eval, parsing
# positionnel strict, rejet de tout octet non imprimable, whitelist de
# types de clés, re-validation par ssh-keygen -lf, verrou unique sur
# authorized_keys, édition temp+mv avec backup .bak 600 unique, ligne
# installée bornée à permitopen=127.0.0.1:<rport>, jamais de secret ni
# de commande brute dans le log.

PATH=/usr/bin:/bin
export PATH
IFS=' '
LC_ALL=C
export LC_ALL
umask 077

D="$HOME/.agentcontrol"
AK="$HOME/.ssh/authorized_keys"
LOCK="$D/pair.lock"
LOG="$D/pair.log"
NONCE=""
STATE=""

log() {
    # ts nonce event … — jamais de clé, jamais la commande brute.
    printf '%s %s %s\n' "$(date +%s)" "${NONCE:-?}" "$*" >> "$LOG" 2>/dev/null || return 0
    # borne : les 200 dernières lignes conservées
    n=$(wc -l < "$LOG" 2>/dev/null || echo 0)
    if [ "${n:-0}" -gt 400 ] 2>/dev/null; then
        tail -n 200 "$LOG" > "$LOG.$$" 2>/dev/null && mv "$LOG.$$" "$LOG"
    fi
}

die() {
    log "refused:$1"
    echo "pair-refused:$1" >&2
    exit 1
}

# ---- verrou unique : flock -w si dispo, sinon mkdir+PID+stale ----
# Tous les chemins qui touchent authorized_keys passent par là (pair,
# revoke, install, sweep) — une installation est transactionnelle.

lock_take() {
    mkdir -p "$D" 2>/dev/null
    if command -v flock >/dev/null 2>&1; then
        exec 9>"$LOCK"
        flock -w 10 9 2>/dev/null || return 1
    else
        ld="$LOCK.d"
        i=0
        while [ $i -lt 20 ]; do
            if mkdir "$ld" 2>/dev/null; then
                printf '%s' $$ > "$ld/pid"
                return 0
            fi
            p=$(cat "$ld/pid" 2>/dev/null)
            if [ -n "$p" ]; then
                kill -0 "$p" 2>/dev/null || { rm -rf "$ld"; continue; }
            elif [ -n "$(find "$ld" -mmin +2 2>/dev/null)" ]; then
                rm -rf "$ld"; continue
            fi
            sleep 1
            i=$((i + 1))
        done
        return 1
    fi
}

lock_drop() {
    if command -v flock >/dev/null 2>&1; then
        exec 9>&- 2>/dev/null || true
    else
        rm -rf "$LOCK.d" 2>/dev/null || true
    fi
}

# ---- retrait de MA ligne K : match ancré en fin de commentaire,
#      backup .bak 600 unique, temp+mv dans ~/.ssh (même filesystem,
#      rename atomique, contexte SELinux hérité + restorecon) ----

remove_k_line() {
    [ -f "$AK" ] || return 0
    [ -L "$AK" ] && return 1
    cp -p "$AK" "$AK.bak" 2>/dev/null && chmod 600 "$AK.bak" 2>/dev/null
    grep -v "agentcontrol-pair-tmp ${NONCE}\$" "$AK" > "$AK.tmp.$$" 2>/dev/null
    rc=$?
    if [ "$rc" -gt 1 ]; then
        rm -f "$AK.tmp.$$"
        return 1
    fi
    chmod 600 "$AK.tmp.$$" 2>/dev/null
    if ! mv "$AK.tmp.$$" "$AK"; then
        rm -f "$AK.tmp.$$"
        return 1
    fi
    command -v restorecon >/dev/null 2>&1 && restorecon "$AK" 2>/dev/null
    return 0
}

# =====================================================================
# --sweep : ménage périodique (cron user ou hook d'install) — lignes K
# dont l'état est expiré, orphelines (>24 h sans .exp), .exp orphelins.
# =====================================================================

if [ "${1:-}" = "--sweep" ]; then
    [ -d "$D" ] || exit 0
    lock_take || exit 0
    now=$(date +%s)
    if [ -f "$AK" ]; then
        # états expirés → retrait de la ligne K correspondante
        for s in "$D"/pair-*.exp; do
            [ -f "$s" ] || continue
            n=${s##*/pair-}
            n=${n%.exp}
            case "$n" in ''|*[!0-9a-f]*) continue ;; esac
            if [ "$(cat "$s" 2>/dev/null || echo 0)" -le "$now" ] 2>/dev/null; then
                NONCE=$n
                remove_k_line
                rm -f "$s" "$D/pair-$n.seen"
                log "sweep-expired"
            fi
        done
        # lignes K sans .exp : vues une fois (.seen), retirées >24 h après
        grep 'agentcontrol-pair-tmp ' "$AK" 2>/dev/null | while IFS= read -r l; do
            n=${l##*agentcontrol-pair-tmp }
            case "$n" in ''|*[!0-9a-f]*) continue ;; esac
            [ -f "$D/pair-$n.exp" ] && continue
            s="$D/pair-$n.seen"
            if [ ! -f "$s" ]; then
                touch "$s"
                continue
            fi
            if [ -n "$(find "$s" -mmin +1440 2>/dev/null)" ]; then
                NONCE=$n
                remove_k_line
                rm -f "$s"
                log "sweep-orphan"
            fi
        done
        # .exp orphelins (ligne K déjà partie sans passer par ici)
        for s in "$D"/pair-*.exp; do
            [ -f "$s" ] || continue
            n=${s##*/pair-}
            n=${n%.exp}
            grep -q "agentcontrol-pair-tmp ${n}\$" "$AK" 2>/dev/null || rm -f "$s"
        done
    else
        # pas d'authorized_keys → aucune ligne possible : purge des états
        rm -f "$D"/pair-*.exp "$D"/pair-*.seen 2>/dev/null
    fi
    lock_drop
    exit 0
fi

# =====================================================================
# Forced command : <nonce> <rport> en arguments (écrits dans la ligne
# authorized_keys à l'install — jamais fournis par le client).
# =====================================================================

NONCE="$1"
RPORT="$2"
[ -n "$NONCE" ] || exit 1
case "$NONCE" in ''|*[!0-9a-f]*) exit 1 ;; esac
case "$RPORT" in ''|*[!0-9]*) exit 1 ;; esac
[ "$RPORT" -ge 1 ] && [ "$RPORT" -le 65535 ] || exit 1
STATE="$D/pair-$NONCE.exp"

# authorized_keys symlinké (setups managés) → refus net, pas de
# remplacement du lien : bascule mode manuel côté client.
[ -L "$AK" ] && die managed

lock_take || die lock

# 1. Expiration — état écrit à l'install, heure SERVEUR autoritaire.
if [ ! -f "$STATE" ] || [ "$(date +%s)" -ge "$(cat "$STATE" 2>/dev/null || echo 0)" ] 2>/dev/null; then
    remove_k_line
    rm -f "$STATE"
    log expired
    lock_drop
    exit 1
fi

# 2. One-shot transactionnel : MA ligne K est-elle encore présente ?
#    (deux exec parallèles sur une session → le perdant sort ici)
grep -q "agentcontrol-pair-tmp ${NONCE}\$" "$AK" 2>/dev/null || {
    lock_drop
    die gone
}

# 3. Parse SSH_ORIGINAL_COMMAND — positionnel strict, rejet de tout
#    octet non imprimable (CR/LF/NUL/contrôle = injection de lignes).
#    Le check est un glob `case` — insensible aux sauts de ligne
#    contrairement à `grep .` qui évalue ligne par ligne.
cmd=${SSH_ORIGINAL_COMMAND:-}
[ -n "$cmd" ] || { lock_drop; die empty; }
case "$cmd" in *[!\ -~]*) lock_drop; die ctl ;; esac
# Découpage sur IFS=' ' : noglob local — un `*` dans la commande ne doit
# pas s'expanser en noms de fichiers (fuite + contournement de charset).
set -f
set -- $cmd
set +f

if [ "${1:-}" = "agentcontrol-pair-revoke" ] && [ $# -eq 1 ]; then
    remove_k_line
    rm -f "$STATE"
    log revoked
    lock_drop
    exit 0
fi

[ "${1:-}" = "agentcontrol-pair" ] && [ $# -eq 4 ] || { lock_drop; die cmd; }
TYPE=$2
B64=$3
LABEL=$4
case "$TYPE" in
    ssh-ed25519 | \
    ecdsa-sha2-nistp256 | ecdsa-sha2-nistp384 | ecdsa-sha2-nistp521 | \
    sk-ssh-ed25519@openssh.com | sk-ecdsa-sha2-nistp256@openssh.com) ;;
    *) lock_drop; die type ;;
esac
# Charset+longueur via `case` (globs insensibles aux sauts de ligne —
# `grep -E` évalue ligne par ligne et un `\n` ferait passer le second
# segment sous le radar).
case "$B64" in ''|*[!A-Za-z0-9+/=]*) lock_drop; die b64 ;; esac
[ "${#B64}" -ge 40 ] && [ "${#B64}" -le 800 ] || { lock_drop; die b64; }
case "$LABEL" in ''|*[!A-Za-z0-9._-]*) lock_drop; die label ;; esac
[ "${#LABEL}" -le 32 ] || { lock_drop; die label; }

# Re-validation réelle du blob : ssh-keygen -lf sur fichier temp —
# rejet si KO ; absent → charset+longueur seuls (tracé dans le log).
KT="$D/.pair-key.$$"
printf '%s %s\n' "$TYPE" "$B64" > "$KT"
if command -v ssh-keygen >/dev/null 2>&1; then
    FP=$(ssh-keygen -lf "$KT" 2>/dev/null | awk '{print $2}')
    if [ -z "$FP" ]; then
        rm -f "$KT"
        lock_drop
        die key
    fi
else
    FP="nokeygen:$(printf '%s' "$B64" | cut -c1-16)"
fi
rm -f "$KT"

# 4. Append de la ligne installée — commentaire construit ICI (jamais
#    repris du client), forwarding borné au démon loopback.
printf 'restrict,port-forwarding,permitopen="127.0.0.1:%s" %s %s agentcontrol-pair %s %s\n' \
    "$RPORT" "$TYPE" "$B64" "$LABEL" "$(date +%Y%m%d-%H%M%S)" >> "$AK" || {
    lock_drop
    die append
}

# 5. Retrait de MA ligne K — auto-destruction one-shot réelle.
remove_k_line || { lock_drop; die remove; }
rm -f "$STATE" "$D/pair-$NONCE.seen"
log "pair-ok $FP $LABEL"
lock_drop
echo "pair-ok:$FP"
exit 0
