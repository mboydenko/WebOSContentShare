/* Server API and small helpers. ES5 only: old webOS TVs run Chromium 38. */

function $(id) { return document.getElementById(id); }

function esc(s) {
  return String(s === undefined || s === null ? '' : s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function pad2(n) { return n < 10 ? '0' + n : '' + n; }

function fmtTime(s) {
  s = Math.max(0, Math.floor(s || 0));
  var h = Math.floor(s / 3600), m = Math.floor(s % 3600 / 60), sec = s % 60;
  return h ? h + ':' + pad2(m) + ':' + pad2(sec) : m + ':' + pad2(sec);
}

function fmtSize(b) {
  if (!b) return '';
  var units = ['Б', 'КБ', 'МБ', 'ГБ', 'ТБ'], i = 0;
  while (b >= 1024 && i < units.length - 1) { b /= 1024; i++; }
  return (i >= 2 ? b.toFixed(1) : Math.round(b)) + ' ' + units[i];
}

function qs(params) {
  var parts = [];
  for (var k in params) {
    if (params.hasOwnProperty(k) && params[k] !== undefined && params[k] !== null) {
      parts.push(encodeURIComponent(k) + '=' + encodeURIComponent(params[k]));
    }
  }
  return parts.join('&');
}

var Store = {
  get: function (k, def) {
    try { var v = window.localStorage.getItem(k); return v === null ? def : v; } catch (e) { return def; }
  },
  set: function (k, v) { try { window.localStorage.setItem(k, v); } catch (e) {} },
  remove: function (k) { try { window.localStorage.removeItem(k); } catch (e) {} }
};

function clientId() {
  var id = Store.get('clientId');
  if (!id) {
    id = 'tv-' + Math.random().toString(36).slice(2) + Date.now().toString(36);
    Store.set('clientId', id);
  }
  return id;
}

var Api = {
  base: '',

  /* cb(err, data). Returns the XHR so callers can abort it. */
  request: function (url, cb, opts) {
    opts = opts || {};
    var xhr = new XMLHttpRequest(), done = false;
    function finish(err, data) { if (!done) { done = true; cb(err, data); } }
    xhr.open('GET', url.indexOf('http') === 0 ? url : Api.base + url, true);
    xhr.timeout = opts.timeout || 30000;
    xhr.onload = function () {
      if (xhr.status >= 200 && xhr.status < 300) {
        if (opts.text) return finish(null, xhr.responseText);
        if (xhr.status === 204 || !xhr.responseText) return finish(null, null);
        try { finish(null, JSON.parse(xhr.responseText)); } catch (e) { finish(new Error('Неверный ответ сервера')); }
      } else {
        var msg = 'Ошибка ' + xhr.status;
        try { msg = JSON.parse(xhr.responseText).error || msg; } catch (e) {}
        finish(new Error(msg));
      }
    };
    xhr.onerror = function () { finish(new Error('Сервер недоступен')); };
    xhr.ontimeout = function () { finish(new Error('Сервер не отвечает')); };
    xhr.send();
    return xhr;
  },

  get: function (url, cb, timeout) { return Api.request(url, cb, { timeout: timeout }); },
  getText: function (url, cb, timeout) { return Api.request(url, cb, { timeout: timeout, text: true }); },

  /* Path-style URL so the TV sees the real file extension. */
  rawUrl: function (root, path) {
    var parts = path.split('/'), out = [];
    for (var i = 0; i < parts.length; i++) out.push(encodeURIComponent(parts[i]));
    return Api.base + '/api/raw/' + encodeURIComponent(root) + '/' + out.join('/');
  },

  thumbUrl: function (root, path, size) {
    return Api.base + '/api/thumb?' + qs({ root: root, path: path, size: size || 320 });
  }
};

/* Saved playback positions, so a film can be resumed later. */
var Resume = {
  key: function (item) { return 'pos:' + item.root + ':' + item.path; },
  get: function (item) {
    var v = parseFloat(Store.get(Resume.key(item), '0'));
    return v > 30 ? v : 0;
  },
  set: function (item, pos, dur) {
    if (!dur || pos < 30 || pos > dur * 0.95) Store.remove(Resume.key(item));
    else Store.set(Resume.key(item), String(Math.floor(pos)));
  }
};
