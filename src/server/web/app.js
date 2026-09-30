// The dashboard. It only reads from the hub and draws what it is given. Everything it shows can be written by an agent, so
// it is only ever put on the page as text (textContent), never as HTML.
(function () {
  "use strict";
  var token = sessionStorage.getItem("cc-token") || "";
  var m = /token=([^&]+)/.exec(location.hash);
  if (m) { token = decodeURIComponent(m[1]); sessionStorage.setItem("cc-token", token); history.replaceState(null, "", location.pathname); }
  var project = sessionStorage.getItem("cc-project") || "";
  var $ = function (id) { return document.getElementById(id); };

  function el(tag, cls, text) { var e = document.createElement(tag); if (cls) e.className = cls; if (text !== undefined) e.textContent = text; return e; }
  function row(left, right, cls) { var r = el("div", "row" + (cls ? " " + cls : "")); r.appendChild(el("span", "", left)); r.appendChild(el("span", "dim", right || "")); return r; }
  function fill(id, nodes, empty) { var box = $(id); box.replaceChildren.apply(box, nodes.length ? nodes : [el("div", "dim", empty)]); }
  function api(path) {
    return fetch(path, { headers: { authorization: "Bearer " + token }, cache: "no-store" }).then(function (r) {
      if (r.status === 401) { throw new Error("denied"); }
      return r.json();
    });
  }

  function show(state) {
    $("conn").textContent = "live";
    var names = Object.keys(state.projects).sort();
    if (!project || names.indexOf(project) < 0) { project = names[0] || ""; }
    $("projects").replaceChildren.apply($("projects"), names.map(function (n) {
      var b = el("button", "", n); if (n === project) { b.setAttribute("aria-current", "true"); }
      b.addEventListener("click", function () { project = n; sessionStorage.setItem("cc-project", n); refresh(); });
      return b;
    }));
    fill("devices", state.devices.map(function (d) {
      return row(d.node, d.agents + " agent(s)" + (d.max ? " of " + d.max : "") + (d.labels.length ? " [" + d.labels.join(",") + "]" : ""), d.connected ? "on" : "off");
    }), "No machines yet.");
    var p = state.projects[project] || { agents: [], asks: [], perms: [], tasks: [] };
    fill("agents", p.agents.map(function (a) { return row(a.name + (a.lead ? " (lead)" : ""), a.status + " on " + a.node); }), "No agents here.");
    var waiting = p.asks.map(function (q) { return row(q.id + " " + q.agent + ": " + q.question, "question"); })
      .concat(p.perms.map(function (x) { return row(x.id + " " + x.agent + ": " + x.kind + " " + x.action, "permission"); }));
    fill("waiting", waiting, "Nothing is waiting for anyone.");
    fill("tasks", p.tasks.map(function (t) { return row(t.id + " " + t.to + ": " + t.text, t.state); }), "No tasks.");
  }

  function showHistory(rows) {
    fill("history", rows.map(function (r) {
      var d = el("div", "msg"); d.appendChild(el("b", "", r.from)); d.appendChild(el("span", "dim", " (" + r.kind + (r.thread ? ", " + r.thread : "") + ")  ")); d.appendChild(document.createTextNode(r.text)); return d;
    }), "No conversation yet.");
  }

  function refresh() {
    api("/api/v1/state").then(function (s) {
      $("login").hidden = true; $("app").hidden = false; show(s);
      if (project) { return api("/api/v1/history?project=" + encodeURIComponent(project) + "&limit=60&latest=1").then(showHistory); }
    }).catch(function (e) {
      $("conn").textContent = e.message === "denied" ? "sign in" : "offline";
      if (e.message === "denied") { sessionStorage.removeItem("cc-token"); token = ""; $("login").hidden = false; $("app").hidden = true; }
    });
  }

  $("go").addEventListener("click", function () { token = $("token").value.trim(); sessionStorage.setItem("cc-token", token); refresh(); });
  refresh();
  setInterval(refresh, 2500);
})();
