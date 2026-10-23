/* Server discovery: scans the TV's /24 subnet for /api/info, or takes a manual address. */

var DEFAULT_PORT = 8200;

/* TV's own IP via the webOS connection manager service (null outside webOS). */
function getLocalIp(cb) {
  var done = false;
  function finish(ip) { if (!done) { done = true; cb(ip); } }
  setTimeout(function () { finish(null); }, 4000);
  if (typeof window.PalmServiceBridge === 'undefined') {
    var h = location.hostname;
    return finish(/^\d+\.\d+\.\d+\.\d+$/.test(h) ? h : null);
  }
  var uris = ['luna://com.webos.service.connectionmanager/getStatus', 'luna://com.palm.connectionmanager/getStatus'];
  function tryNext() {
    var uri = uris.shift();
    if (!uri) return finish(null);
    try {
      var bridge = new window.PalmServiceBridge();
      window._ipBridge = bridge; // keep a reference so the callback is not collected
      bridge.onservicecallback = function (msg) {
        try {
          var r = JSON.parse(msg), ip = null;
          if (r.wired && r.wired.state === 'connected') ip = r.wired.ipAddress;
          if (!ip && r.wifi && r.wifi.state === 'connected') ip = r.wifi.ipAddress;
          if (ip) finish(ip); else tryNext();
        } catch (e) { tryNext(); }
      };
      bridge.call(uri, '{}');
    } catch (e) { tryNext(); }
  }
  tryNext();
}

function probeServer(base, cb, timeout) {
  Api.request(base + '/api/info', function (err, d) {
    cb(!err && d && d.app === 'webos-content-share' ? { base: base, name: d.name } : null);
  }, { timeout: timeout || 1500 });
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
  scanned: false,

  onShow: function () {
    Servers.render();
    var saved = Store.get('server');
    if (saved && !$('server-input').value) $('server-input').value = saved.replace(/^https?:\/\//, '');
    Nav.setScope(Servers.el);
    if (!Servers.scanned) Servers.scan();
  },

  onKey: function (code) {
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
    for (var i = 0; i < Servers.found.length; i++) if (Servers.found[i].base === s.base) return;
    Servers.found.push(s);
    var focused = Nav.current();
    Servers.render();
    if (App.screen === Servers && (!focused || focused.className.indexOf('server') >= 0)) {
      Nav.setScope(Servers.el, $('server-list').firstChild);
    }
  },

  scan: function () {
    if (Servers.scanning) return;
    Servers.scanning = true;
    Servers.scanned = true;
    Servers.status('Определяю адрес телевизора…');
    var saved = Store.get('server');
    if (saved) probeServer(saved, function (s) { if (s) Servers.addFound(s); }, 3000);

    getLocalIp(function (ip) {
      if (!ip) {
        Servers.scanning = false;
        Servers.status('Не удалось определить сеть. Введите адрес сервера вручную.');
        return;
      }
      var prefix = ip.split('.').slice(0, 3).join('.') + '.';
      var queue = [], total, done = 0, active = 0;
      for (var i = 1; i < 255; i++) queue.push(prefix + i);
      total = queue.length;
      Servers.status('Ищу серверы в сети ' + prefix + '0…');
      function next() {
        while (active < 24 && queue.length) {
          active++;
          probeServer('http://' + queue.shift() + ':' + DEFAULT_PORT, function (s) {
            active--; done++;
            if (s) Servers.addFound(s);
            if (done === total) {
              Servers.scanning = false;
              Servers.status(Servers.found.length ? 'Выберите сервер.' :
                'Серверы не найдены. Убедитесь, что сервер запущен на ПК и брандмауэр разрешает порт ' + DEFAULT_PORT + '.');
            } else {
              Servers.status('Поиск серверов… ' + Math.round(done * 100 / total) + '%');
              next();
            }
          });
        }
      }
      next();
    });
  },

  connectManual: function () {
    var base = normalizeAddress($('server-input').value);
    if (!base) { toast('Введите адрес сервера'); return; }
    $('server-input').blur();
    App.connect(base);
  }
};

$('server-connect').onActivate = Servers.connectManual;
$('server-scan').onActivate = function () { Servers.found = []; Servers.render(); Servers.scan(); };
$('server-input').addEventListener('keydown', function (e) {
  if (e.keyCode === KEY.ENTER) { e.preventDefault(); Servers.connectManual(); }
});
