/*
 * Server discovery, fastest first:
 *   1. the last used server;
 *   2. UDP broadcast through the bundled webOS service (finds any port, ~1.5 s);
 *   3. HTTP scan of the TV's /24 subnet on the default port (works without the service).
 * When the user is not interacting, the app connects by itself to the
 * previously used server (matched by id, so a changed IP is fine) or to the only one found.
 */

var DEFAULT_PORT = 8200;
var DISCOVERY_SERVICE = 'luna://com.contentshare.player.discovery/discover';
var RESCAN_DELAY = 15000;

/* Calls a webOS system/app service. cb(err, response). */
function lunaCall(uri, params, cb, timeout) {
  var done = false;
  function finish(err, res) { if (!done) { done = true; cb(err, res); } }
  if (typeof window.PalmServiceBridge === 'undefined') return finish(new Error('Службы webOS недоступны'));
  setTimeout(function () { finish(new Error('Служба не ответила: ' + uri)); }, timeout || 5000);
  try {
    var bridge = new window.PalmServiceBridge();
    lunaCall.alive.push(bridge); // keep a reference so the callback is not garbage-collected
    bridge.onservicecallback = function (msg) {
      var i = lunaCall.alive.indexOf(bridge);
      if (i >= 0) lunaCall.alive.splice(i, 1);
      var r;
      try { r = JSON.parse(msg); } catch (e) { return finish(new Error('Неверный ответ службы')); }
      if (r.returnValue === false) finish(new Error(r.errorText || 'Ошибка службы'));
      else finish(null, r);
    };
    bridge.call(uri, JSON.stringify(params || {}));
  } catch (e) {
    finish(e);
  }
}
lunaCall.alive = [];

/* TV's own IP (null outside webOS unless the page was opened by IP). */
function getLocalIp(cb) {
  if (typeof window.PalmServiceBridge === 'undefined') {
    var h = location.hostname;
    return cb(/^\d+\.\d+\.\d+\.\d+$/.test(h) ? h : null);
  }
  var uris = ['luna://com.webos.service.connectionmanager/getStatus', 'luna://com.palm.connectionmanager/getStatus'];
  (function tryNext() {
    var uri = uris.shift();
    if (!uri) return cb(null);
    lunaCall(uri, {}, function (err, r) {
      var ip = null;
      if (!err) {
        if (r.wired && r.wired.state === 'connected') ip = r.wired.ipAddress;
        if (!ip && r.wifi && r.wifi.state === 'connected') ip = r.wifi.ipAddress;
      }
      if (ip) cb(ip); else tryNext();
    }, 4000);
  })();
}

function probeServer(base, cb, timeout) {
  Api.request(base + '/api/info', function (err, d) {
    cb(!err && d && d.app === 'webos-content-share' ? { base: base, name: d.name, id: d.id || '' } : null);
  }, { timeout: timeout || 1500 });
}

/* cb(err, [{base, name, id}]) — servers that answered the UDP broadcast. */
function discoverUdp(cb) {
  // The first call may cold-start the Node.js service, hence the generous timeout.
  lunaCall(DISCOVERY_SERVICE, { timeout: 1500 }, function (err, r) {
    if (err) return cb(err);
    var out = [], list = r.servers || [];
    for (var i = 0; i < list.length; i++) {
      out.push({ base: 'http://' + list[i].address + ':' + list[i].port, name: list[i].name, id: list[i].id });
    }
    cb(null, out);
  }, 10000);
}

function normalizeAddress(text) {
  text = (text || '').replace(/\s+/g, '').replace(/\/+$/, '');
  if (!text) return '';
  if (!/^https?:\/\//.test(text)) text = 'http://' + text;
  if (!/:\d+$/.test(text.replace(/^https?:\/\//, ''))) text += ':' + DEFAULT_PORT;
  return text;
}

var Servers = {
  el: $('screen-servers'),
  found: [],
  scanning: false,
  scanGen: 0,
  lastScan: 0,
  rescanTimer: null,
  /* Connect without asking: set at startup, cleared as soon as the user presses a key here. */
  autoConnect: false,

  onShow: function () {
    $('servers-back').className = 'btn back focusable' + (Api.base && Browser.ready ? '' : ' hidden');
    Servers.render();
    var saved = Store.get('server');
    if (saved && !$('server-input').value) $('server-input').value = saved.replace(/^https?:\/\//, '');
    Nav.setScope(Servers.el, $('server-list').firstChild);
    if (!Servers.scanning && Date.now() - Servers.lastScan > 20000) Servers.rescan();
  },

  onHide: function () {
    clearTimeout(Servers.rescanTimer);
    Servers.scanGen++; // results of a running scan are no longer needed
    Servers.scanning = false;
    Servers.autoConnect = false;
  },

  onKey: function (code) {
    Servers.autoConnect = false;
    if (isBackKey(code) && document.activeElement !== $('server-input')) {
      if (Api.base && Browser.ready) App.show(Browser);
      else Dialog.open('Выйти из приложения?', [{ label: 'Выйти', action: exitApp }, { label: 'Отмена' }]);
      return true;
    }
    return false;
  },

  status: function (text) { $('scan-status').textContent = text; },

  render: function () {
    var box = $('server-list');
    box.innerHTML = '';
    for (var i = 0; i < Servers.found.length; i++) {
      (function (s) {
        var el = document.createElement('div');
        el.className = 'server focusable';
        el.innerHTML = '<div class="s-name">' + esc(s.name) + '</div><div class="s-addr">' + esc(s.base.replace('http://', '')) + '</div>';
        el.onActivate = function () { App.connect(s.base); };
        box.appendChild(el);
      })(Servers.found[i]);
    }
  },

  addFound: function (s) {
    for (var i = 0; i < Servers.found.length; i++) {
      var f = Servers.found[i];
      if (f.base === s.base || (s.id && f.id === s.id)) return;
    }
    Servers.found.push(s);
    if (Servers.autoConnect && s.id && s.id === Store.get('serverId')) {
      Servers.autoConnect = false;
      App.connect(s.base);
      return;
    }
    var focused = Nav.current();
    Servers.render();
    var movable = !focused || focused.className.indexOf('server') >= 0 || focused === $('servers-back');
    if (App.screen === Servers && movable) Nav.setScope(Servers.el, $('server-list').firstChild);
    if (!Servers.scanning) Servers.settle(); // e.g. the saved server answered after the scan ended
  },

  rescan: function () {
    Servers.found = [];
    Servers.render();
    Servers.scan();
  },

  scan: function () {
    if (Servers.scanning) return;
    clearTimeout(Servers.rescanTimer);
    Servers.scanning = true;
    var gen = ++Servers.scanGen;
    function current() { return gen === Servers.scanGen; }
    Servers.status('Ищу серверы в локальной сети…');

    var saved = Store.get('server');
    if (saved) probeServer(saved, function (s) { if (s && current()) Servers.addFound(s); }, 3000);

    discoverUdp(function (err, list) {
      if (!current()) return;
      if (err || !list.length) { Servers.scanSubnet(gen); return; }
      // Confirm over HTTP: a reply to UDP does not prove the TCP port is open in the firewall.
      var pending = list.length;
      for (var i = 0; i < list.length; i++) {
        probeServer(list[i].base, function (s) {
          if (!current()) return;
          if (s) Servers.addFound(s);
          if (--pending === 0) {
            if (Servers.found.length) Servers.finishScan();
            else Servers.scanSubnet(gen);
          }
        }, 3000);
      }
    });
  },

  scanSubnet: function (gen) {
    function current() { return gen === Servers.scanGen; }
    Servers.status('Определяю адрес телевизора…');
    getLocalIp(function (ip) {
      if (!current()) return;
      if (!ip) { Servers.finishScan(); return; }
      var prefix = ip.split('.').slice(0, 3).join('.') + '.';
      var queue = [], total, done = 0, active = 0;
      for (var i = 1; i < 255; i++) queue.push(prefix + i);
      total = queue.length;
      Servers.status('Ищу серверы в сети ' + prefix + '0…');
      (function next() {
        while (active < 24 && queue.length) {
          active++;
          probeServer('http://' + queue.shift() + ':' + DEFAULT_PORT, function (s) {
            if (!current()) return;
            active--; done++;
            if (s) Servers.addFound(s);
            if (done === total) Servers.finishScan();
            else { Servers.status('Поиск серверов… ' + Math.round(done * 100 / total) + '%'); next(); }
          });
        }
      })();
    });
  },

  finishScan: function () {
    Servers.scanning = false;
    Servers.lastScan = Date.now();
    Servers.settle();
  },

  /* Status line, auto-connect and periodic re-scan, based on what has been found so far. */
  settle: function () {
    clearTimeout(Servers.rescanTimer);
    if (App.screen !== Servers) return;
    var n = Servers.found.length;
    if (n === 1 && Servers.autoConnect) {
      Servers.autoConnect = false;
      App.connect(Servers.found[0].base);
      return;
    }
    if (n) {
      Servers.status(n === 1 ? 'Найден сервер. Нажмите OK, чтобы подключиться.' : 'Найдено серверов: ' + n + '. Выберите нужный.');
      return;
    }
    Servers.status('Серверы не найдены — продолжаю искать. Убедитесь, что сервер запущен на компьютере ' +
      'и брандмауэр разрешает порты ' + DEFAULT_PORT + ' (TCP) и 8201 (UDP).');
    Servers.rescanTimer = setTimeout(function () {
      if (App.screen === Servers && !Servers.scanning) Servers.scan();
    }, RESCAN_DELAY);
  },

  connectManual: function () {
    var base = normalizeAddress($('server-input').value);
    if (!base) { toast('Введите адрес сервера'); return; }
    $('server-input').blur();
    App.connect(base);
  }
};

$('server-connect').onActivate = Servers.connectManual;
$('server-scan').onActivate = Servers.rescan;
$('server-input').addEventListener('focus', function () { Servers.autoConnect = false; });
$('server-input').addEventListener('keydown', function (e) {
  if (e.keyCode === KEY.ENTER) { e.preventDefault(); Servers.connectManual(); }
});
