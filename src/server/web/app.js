// The dashboard. It only reads from the hub and draws what it is given. Everything it shows can be written by an agent, so it is
// only ever put on the page as text (textContent and SVG text), never as HTML, and it uses no inline styles or scripts.
(function () {
  "use strict";
  var token = sessionStorage.getItem("cc-token") || "";
  var m = /token=([^&]+)/.exec(location.hash);
  if (m) { token = decodeURIComponent(m[1]); sessionStorage.setItem("cc-token", token); history.replaceState(null, "", location.pathname); }
  var project = sessionStorage.getItem("cc-project") || "";
  var page = 0;
  var last = null;
  var $ = function (id) { return document.getElementById(id); };
  var SVG = "http://www.w3.org/2000/svg";

  function el(tag, cls, text) { var e = document.createElement(tag); if (cls) e.className = cls; if (text !== undefined) e.textContent = text; return e; }
  function sv(tag, attrs, text) {
    var e = document.createElementNS(SVG, tag);
    Object.keys(attrs || {}).forEach(function (k) { e.setAttribute(k, attrs[k]); });
    if (text !== undefined) e.textContent = text;
    return e;
  }
  function row(left, right, cls) { var r = el("div", "row" + (cls ? " " + cls : "")); r.appendChild(el("span", "", left)); r.appendChild(el("span", "dim", right || "")); return r; }
  function fill(id, nodes, empty) { var box = $(id); box.replaceChildren.apply(box, nodes.length ? nodes : [el("div", "dim", empty)]); }
  function table(id, head, rows, empty) {
    var t = $(id), tr = el("tr");
    head.forEach(function (h) { tr.appendChild(el("th", "", h)); });
    var body = rows.map(function (r) { var x = el("tr"); r.forEach(function (c) { x.appendChild(el("td", "", c)); }); return x; });
    if (!body.length) { var e = el("tr"), td = el("td", "dim", empty); td.setAttribute("colspan", head.length); e.appendChild(td); body = [e]; }
    t.replaceChildren.apply(t, [tr].concat(body));
  }
  function api(path) {
    // A Discord sign-in travels as a cookie; a pasted token as a header.
    return fetch(path, { headers: token ? { authorization: "Bearer " + token } : {}, cache: "no-store" }).then(function (r) {
      if (r.status === 401) { throw new Error("denied"); }
      return r.json();
    });
  }
  var busy = function (s) { return s === "thinking" || s === "executing"; };

  // One boxed node on the canvas: a dark title bar and two lines of text. Returns the group.
  function node(g, x, y, w, title, line1, line2, cls, dot) {
    g.appendChild(sv("rect", { x: x, y: y, width: w, height: 58, class: "node " + (cls || "") }));
    g.appendChild(sv("rect", { x: x, y: y, width: w, height: 18, class: "bar" }));
    g.appendChild(sv("text", { x: x + 8, y: y + 13, class: "bar-text" }, title.toUpperCase()));
    g.appendChild(sv("text", { x: x + 8, y: y + 35 }, line1));
    g.appendChild(sv("text", { x: x + 8, y: y + 51, class: "dim" }, line2 || ""));
    if (dot !== undefined) { g.appendChild(sv("rect", { x: x + w - 20, y: y + 26, width: 10, height: 10, class: "dot" + (dot ? " busy" : "") })); }
  }
  function link(g, x1, y1, x2, y2, off) {
    var mid = (x1 + x2) / 2;
    g.insertBefore(sv("path", { d: "M" + x1 + " " + y1 + " C" + mid + " " + y1 + " " + mid + " " + y2 + " " + x2 + " " + y2, class: "line" + (off ? " off" : "") }), g.firstChild);
  }

  // Draws Discord -> hub -> machines -> agents for the chosen project, laid out in four columns.
  function drawCanvas(state, p) {
    var svg = $("canvas"), g = sv("g");
    var nodes = [];
    p.agents.forEach(function (a) { if (nodes.indexOf(a.node) < 0) { nodes.push(a.node); } });
    var rows = Math.max(p.agents.length, nodes.length, 1), H = 40 + rows * 76;
    svg.setAttribute("viewBox", "0 0 1040 " + H);
    for (var x = 0; x <= 1040; x += 24) { g.appendChild(sv("line", { x1: x, y1: 0, x2: x, y2: H, class: "grid" })); }
    for (var y = 0; y <= H; y += 24) { g.appendChild(sv("line", { x1: 0, y1: y, x2: 1040, y2: y, class: "grid" })); }
    var mid = H / 2 - 29, agentY = {};
    node(g, 16, mid, 200, "discord", "#" + project, "you and your team");
    node(g, 280, mid, 200, "hub", "routes + remembers", p.asks.length + p.perms.length + " waiting");
    link(g, 216, mid + 29, 280, mid + 29);
    var byNode = {};
    p.agents.forEach(function (a, i) { agentY[a.name] = 20 + i * 76; (byNode[a.node] = byNode[a.node] || []).push(agentY[a.name]); });
    nodes.forEach(function (n, i) {
      var ys = byNode[n], cy = ys.reduce(function (s, v) { return s + v; }, 0) / ys.length;
      var d = state.devices.filter(function (x) { return x.node === n; })[0] || { connected: false, agents: 0 };
      node(g, 544, cy, 200, "machine", n, d.agents + " agent(s)" + (d.connected ? "" : ", offline"), d.connected ? "" : "off", d.connected ? 1 : 0);
      link(g, 480, mid + 29, 544, cy + 29, !d.connected);
    });
    p.agents.forEach(function (a) {
      var cy = agentY[a.name];
      node(g, 808, cy, 216, a.lead ? "agent (lead)" : "agent", a.name, a.status, a.status === "offline" ? "off" : "", busy(a.status) ? 1 : 0);
      link(g, 744, (byNode[a.node].reduce(function (s, v) { return s + v; }, 0) / byNode[a.node].length) + 29, 808, cy + 29, a.status === "offline");
    });
    svg.replaceChildren(g);
  }

  function show(state) {
    last = state;
    $("conn").textContent = "live";
    var names = Object.keys(state.projects).sort();
    if (!project || names.indexOf(project) < 0) { project = names[0] || ""; }
    $("projects").replaceChildren.apply($("projects"), names.map(function (n) {
      var b = el("button", "", n); if (n === project) { b.setAttribute("aria-current", "true"); }
      b.addEventListener("click", function () { project = n; page = 0; sessionStorage.setItem("cc-project", n); refresh(); });
      return b;
    }));
    var p = state.projects[project] || { agents: [], asks: [], perms: [], tasks: [] };
    var allAgents = names.reduce(function (n, k) { return n + state.projects[k].agents.length; }, 0);
    var busyAgents = names.reduce(function (n, k) { return n + state.projects[k].agents.filter(function (a) { return busy(a.status); }).length; }, 0);
    var online = state.devices.filter(function (d) { return d.connected; }).length;
    var done = p.tasks.filter(function (t) { return t.state === "done"; }).length;
    var counters = [["machines", state.devices.length, online + " online"], ["agents", allAgents, busyAgents + " busy"], ["projects", names.length, ""], ["tasks here", p.tasks.length, done + " done"]];
    $("counters").replaceChildren.apply($("counters"), counters.map(function (c) {
      var d = el("div", "box counter"); d.appendChild(el("div", "h", c[0])); d.appendChild(el("div", "n", String(c[1]))); d.appendChild(el("div", "dim", c[2])); return d;
    }));
    drawCanvas(state, p);
    fill("devices", state.devices.map(function (d) {
      return row(d.node, d.agents + " agent(s)" + (d.max ? " of " + d.max : "") + (d.labels.length ? " [" + d.labels.join(",") + "]" : ""), d.connected ? "on" : "off");
    }), "No machines yet.");
    var waiting = p.asks.map(function (q) { return row(q.id + " " + q.agent + ": " + q.question, "question"); })
      .concat(p.perms.map(function (x) { return row(x.id + " " + x.agent + ": " + x.kind + " " + x.action, "permission"); }));
    fill("waiting", waiting, "Nothing is waiting for anyone.");
    table("tasks", ["id", "state", "owner", "task"], p.tasks.map(function (t) { return [t.id, t.state, t.to, t.text]; }), "No tasks.");
    showAgents(p);
  }

  // The agent table, filtered by the search box and cut into pages.
  function showAgents(p) {
    var q = $("search").value.trim().toLowerCase(), size = Number($("size").value);
    var rows = p.agents.filter(function (a) { return !q || (a.name + " " + a.node + " " + a.status).toLowerCase().indexOf(q) >= 0; });
    var pages = Math.max(1, Math.ceil(rows.length / size));
    if (page >= pages) { page = pages - 1; }
    var from = page * size;
    table("agents", ["name", "machine", "status"], rows.slice(from, from + size).map(function (a) { return [a.name + (a.lead ? " *" : ""), a.node, a.status]; }), "No agents here.");
    $("range").textContent = rows.length ? "* lead. showing " + (from + 1) + "-" + Math.min(rows.length, from + size) + " of " + rows.length : "";
    $("prev").disabled = page === 0; $("next").disabled = page >= pages - 1;
  }

  function showHistory(rows) {
    fill("history", rows.map(function (r) {
      var d = el("div", "msg"); d.appendChild(el("b", "", r.from)); d.appendChild(el("span", "dim", " (" + r.kind + (r.thread ? ", " + r.thread : "") + ")  ")); d.appendChild(document.createTextNode(r.text)); return d;
    }), "No conversation yet.");
  }

  // Availability per component over the last day, week and month, and whether it is up now.
  function showUptime(u) {
    var names = Object.keys(u.components || {}).sort();
    var pct = function (w) { return w && w.availability !== null ? (w.availability * 100).toFixed(3) + "%" : "no data"; };
    fill("uptime", names.map(function (n) {
      var c = u.components[n];
      return row(n + " (" + c.state + ")", "24h " + pct(c.windows["24h"]) + "  7d " + pct(c.windows["7d"]) + "  30d " + pct(c.windows["30d"]), c.state === "up" ? "on" : "off");
    }), "Nothing recorded yet.");
  }

  function refresh() {
    api("/api/v1/state").then(function (s) {
      $("login").hidden = true; $("app").hidden = false; show(s);
      api("/api/v1/uptime").then(showUptime).catch(function () {});
      showLog();
      if (project) { return api("/api/v1/history?project=" + encodeURIComponent(project) + "&limit=60&latest=1").then(showHistory); }
    }).catch(function (e) {
      $("conn").textContent = e.message === "denied" ? "sign in" : "offline";
      if (e.message === "denied") { sessionStorage.removeItem("cc-token"); token = ""; $("login").hidden = false; $("app").hidden = true; }
    });
  }

  // The hub's own log, for workspace owners (and dashboard tokens). Anyone else gets a refusal and the panel stays hidden.
  function showLog() {
    var url = "/api/v1/logs?lines=200&level=" + encodeURIComponent($("loglevel").value) + "&q=" + encodeURIComponent($("logq").value.trim());
    api(url).then(function (r) {
      var box = $("hublog"), atEnd = box.scrollTop + box.clientHeight >= box.scrollHeight - 20;
      $("logbox").hidden = false;
      box.textContent = r.lines && r.lines.length ? r.lines.join("\n") : "Nothing matches.";
      if (atEnd) { box.scrollTop = box.scrollHeight; }
    }).catch(function () { $("logbox").hidden = true; });
  }

  // Show who is signed in with Discord (if anyone) and let them sign out.
  function whoami() {
    fetch("/auth/me", { cache: "no-store" }).then(function (r) { return r.ok ? r.json() : null; }).then(function (me) {
      $("who").textContent = me ? me.user : ""; $("out").hidden = !me;
    }).catch(function () {});
  }
  $("out").addEventListener("click", function () { fetch("/auth/logout", { method: "POST" }).then(function () { location.reload(); }); });
  $("go").addEventListener("click", function () { token = $("token").value.trim(); sessionStorage.setItem("cc-token", token); refresh(); });
  ["logq", "loglevel"].forEach(function (id) { $(id).addEventListener("input", showLog); });
  ["search", "size"].forEach(function (id) { $(id).addEventListener("input", function () { page = 0; if (last) { showAgents(last.projects[project]); } }); });
  $("prev").addEventListener("click", function () { page--; if (last) { showAgents(last.projects[project]); } });
  $("next").addEventListener("click", function () { page++; if (last) { showAgents(last.projects[project]); } });
  refresh(); whoami();
  setInterval(refresh, 2500);
})();
