// Plugin démo « hello plugin » — preuve de la chaîne option B :
//   serve (/plugin/<name>/) → iframe sandboxée → ready (bootstrap
//   token) → query → data → rendu.
//
// Contrat bridge (client → plugin / plugin → client) :
//   ← {ac:"ready", token, component}   bootstrap (postMessage, jamais d'URL)
//   ← {ac:"theme", dark}               sync thème
//   → {ac:"query", id, component, token, query}   query déclarée du composant
//   ← {ac:"data", id, component, token, ok, data|error}
//   → {ac:"notify"|"navigate"|"resize"|"open-external", …}
// Tout message sortant porte component+token reçus au `ready` — le
// client les vérifie avant de répondre.

(function () {
  "use strict";

  var bridge = { token: null, component: null, parent: window.parent };
  var seq = 0;
  var pending = {};
  var booted = false;

  function send(msg) {
    // targetOrigin "*" obligatoire : l'iframe sandboxée a une origine
    // opaque — mais contentWindow ne désigne QUE la page parente.
    bridge.parent.postMessage(
      Object.assign({ component: bridge.component, token: bridge.token }, msg),
      "*"
    );
  }

  function query(name) {
    return new Promise(function (resolve, reject) {
      var id = "q" + ++seq;
      pending[id] = { resolve: resolve, reject: reject };
      send({ ac: "query", id: id, query: name });
      setTimeout(function () {
        if (pending[id]) {
          delete pending[id];
          reject(new Error("timeout query " + name));
        }
      }, 20000);
    });
  }

  window.addEventListener("message", function (ev) {
    // Seul le parent légitime parle au plugin — source stricte.
    if (ev.source !== window.parent) return;
    // postMessage = structured clone : ev.data est déjà un objet (une
    // string JSON reste tolérée).
    var m;
    try {
      m = typeof ev.data === "string" ? JSON.parse(ev.data) : ev.data;
    } catch (e) { return; }
    if (!m) return;

    if (m.ac === "ready") {
      if (booted) return;
      booted = true;
      bridge.token = m.token;
      bridge.component = m.component;
      boot();
      return;
    }
    if (m.component !== bridge.component) return;
    if (m.ac === "data") {
      var p = pending[m.id];
      if (!p) return;
      delete pending[m.id];
      if (m.ok) p.resolve(m.data);
      else p.reject(new Error(m.error || "query failed"));
      return;
    }
    if (m.ac === "theme") {
      document.body.classList.toggle("light", !m.dark);
    }
  });

  function boot() {
    document.getElementById("refresh").onclick = load;
    load();
  }

  function load() {
    var status = document.getElementById("status");
    var err = document.getElementById("err");
    var ul = document.getElementById("items");
    status.textContent = "chargement…";
    err.textContent = "";
    ul.innerHTML = "";
    query("items")
      .then(function (raw) {
        var items = JSON.parse(raw);
        status.textContent = items.length + " item(s)";
        items.forEach(function (it) {
          var li = document.createElement("li");
          var title = document.createElement("div");
          title.textContent = "#" + it.id + " — " + (it.title || it.description || "");
          var meta = document.createElement("div");
          meta.className = "meta";
          meta.textContent = [it.status, it.repo, it.priority != null ? "p" + it.priority : ""]
            .filter(Boolean).join(" · ");
          li.appendChild(title);
          li.appendChild(meta);
          ul.appendChild(li);
        });
        if (!items.length) {
          var li = document.createElement("li");
          li.textContent = "file vide";
          ul.appendChild(li);
        }
      })
      .catch(function (e) {
        status.textContent = "";
        err.textContent = String(e && e.message ? e.message : e);
      });
  }
})();
