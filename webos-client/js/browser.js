/* Folder browser: roots -> folders -> files, shown as a grid of tiles. */

var ICONS = {
  dir: '<svg viewBox="0 0 24 24"><path d="M10 4l2 2h8a2 2 0 012 2v10a2 2 0 01-2 2H4a2 2 0 01-2-2V6a2 2 0 012-2h6z"/></svg>',
  video: '<svg viewBox="0 0 24 24"><path d="M4 4h16a2 2 0 012 2v12a2 2 0 01-2 2H4a2 2 0 01-2-2V6a2 2 0 012-2zm6 4v8l6-4-6-4z"/></svg>',
  audio: '<svg viewBox="0 0 24 24"><path d="M12 3v10.6A4 4 0 1014 17V7h4V3h-6z"/></svg>',
  image: '<svg viewBox="0 0 24 24"><path d="M4 4h16a2 2 0 012 2v12a2 2 0 01-2 2H4a2 2 0 01-2-2V6a2 2 0 012-2zm1 13h14l-4.5-6-3.5 4.5-2.5-3L5 17z"/></svg>'
};

var Browser = {
  el: $('screen-browser'),
  ready: false,
  stack: [],        // parent locations: {root, path, focus}
  loc: null,        // {root, path}
  entries: [],
  tiles: [],
  req: null,
  restoreIndex: 0,

  home: function () {
    Browser.stack = [];
    Browser.load(null, '', 0);
    App.show(Browser);
  },

  onShow: function () {
    $('browser-title').textContent = Browser.title || App.serverName || '';
    if (Browser.tiles.length) {
      Nav.setScope(Browser.el, Browser.tiles[Math.min(Browser.restoreIndex, Browser.tiles.length - 1)]);
      Browser.loadThumbs();
    } else {
      Nav.setScope(Browser.el);
    }
  },

  onKey: function (code) {
    if (isBackKey(code)) { Browser.up(); return true; }
    return false;
  },

  focusIndex: function () {
    var i = Browser.tiles.indexOf(Nav.current());
    return i < 0 ? 0 : i;
  },

  open: function (entry) {
    Browser.stack.push({ root: Browser.loc.root, path: Browser.loc.path, focus: Browser.focusIndex() });
    Browser.load(entry.root, entry.path, 0);
  },

  up: function () {
    var prev = Browser.stack.pop();
    if (prev) { Browser.load(prev.root, prev.path, prev.focus); return; }
    Dialog.open('Выйти из приложения?', [{ label: 'Выйти', action: exitApp }, { label: 'Отмена' }]);
  },

  reload: function () { if (Browser.loc) Browser.load(Browser.loc.root, Browser.loc.path, Browser.focusIndex()); },

  load: function (root, path, focus) {
    if (Browser.req) Browser.req.abort();
    Browser.loc = { root: root, path: path };
    $('browser-empty').textContent = 'Загрузка…';
    var url = '/api/browse?' + qs(root ? { root: root, path: path } : {});
    Browser.req = Api.get(url, function (err, d) {
      Browser.req = null;
      if (err) {
        $('browser-grid').innerHTML = '';
        Browser.tiles = [];
        $('browser-empty').textContent = 'Ошибка: ' + err.message;
        if (App.screen === Browser) Nav.setScope(Browser.el);
        Dialog.open('Не удалось открыть папку.\n' + err.message, [
          { label: 'Повторить', action: Browser.reload },
          { label: 'Сменить сервер', action: function () { App.show(Servers); } }
        ]);
        return;
      }
      Browser.ready = true;
      Browser.render(d, focus || 0);
    });
  },

  render: function (d, focus) {
    var grid = $('browser-grid'), entries = d.entries || [];
    Browser.title = d.title;
    Browser.entries = [];
    Browser.tiles = [];
    grid.innerHTML = '';
    $('browser-title').textContent = d.title || App.serverName;
    $('browser-path').textContent = d.root ? (App.serverName + (d.path ? ' / ' + d.path.split('/').join(' / ') : '')) : 'Медиапапки';
    $('browser-scroll').scrollTop = 0;
    $('browser-back').className = 'btn back focusable' + (Browser.stack.length ? '' : ' hidden');

    for (var i = 0; i < entries.length; i++) {
      var e = entries[i];
      e.root = e.root || d.root;
      Browser.entries.push(e);
      Browser.tiles.push(Browser.tile(e));
    }
    var frag = document.createDocumentFragment();
    for (var j = 0; j < Browser.tiles.length; j++) frag.appendChild(Browser.tiles[j]);
    grid.appendChild(frag);

    $('browser-empty').textContent = entries.length ? '' :
      (d.root ? 'В этой папке нет медиафайлов.' : 'На сервере нет медиапапок. Добавьте их в панели управления на компьютере.');
    Browser.restoreIndex = focus;
    if (App.screen === Browser) {
      Nav.setScope(Browser.el, Browser.tiles[Math.min(focus, Browser.tiles.length - 1)]);
    }
    Browser.loadThumbs();
  },

  tile: function (e) {
    var el = document.createElement('div');
    el.className = 'tile focusable ' + e.kind;
    var meta = e.kind === 'dir' ? '' : fmtSize(e.size);
    if (e.kind === 'video') {
      var pos = Resume.get(e);
      if (pos) meta = 'Просмотрено до ' + fmtTime(pos) + ' · ' + meta;
    }
    el.innerHTML = '<div class="thumb">' + (ICONS[e.kind] || ICONS.video) + '</div>' +
      '<div class="name">' + esc(e.name) + '</div>' +
      (meta ? '<div class="meta">' + esc(meta) + '</div>' : '');
    el._entry = e;
    el._wantsThumb = e.kind === 'video' || e.kind === 'audio' || e.kind === 'image';
    el.onActivate = function () { Browser.activateEntry(e); };
    el.onNavFocus = function () { Browser.restoreIndex = Browser.tiles.indexOf(el); Browser.loadThumbs(); };
    return el;
  },

  /* Load previews only for tiles near the visible area. */
  loadThumbs: function () {
    var sc = $('browser-scroll').getBoundingClientRect();
    for (var i = 0; i < Browser.tiles.length; i++) {
      var t = Browser.tiles[i];
      if (!t._wantsThumb || t._thumbLoading) continue;
      var r = t.getBoundingClientRect();
      if (r.bottom < sc.top - 400 || r.top > sc.bottom + 700) continue;
      t._thumbLoading = true;
      (function (tile) {
        var img = document.createElement('img');
        img.onload = function () { img.className = 'loaded'; };
        img.onerror = function () { if (img.parentNode) img.parentNode.removeChild(img); };
        img.src = Api.thumbUrl(tile._entry.root, tile._entry.path, 320);
        tile.firstChild.appendChild(img);
      })(t);
    }
  },

  siblings: function (kind) {
    var out = [];
    for (var i = 0; i < Browser.entries.length; i++) if (Browser.entries[i].kind === kind) out.push(Browser.entries[i]);
    return out;
  },

  activateEntry: function (e) {
    if (e.kind === 'dir') Browser.open(e);
    else if (e.kind === 'video' || e.kind === 'audio') Player.open(e, Browser.siblings(e.kind));
    else if (e.kind === 'image') {
      var imgs = Browser.siblings('image');
      Viewer.open(imgs, imgs.indexOf(e));
    }
  },

  /* Update "watched until" captions after returning from the player. */
  refreshMeta: function () {
    for (var i = 0; i < Browser.tiles.length; i++) {
      var t = Browser.tiles[i], e = t._entry;
      if (e.kind !== 'video') continue;
      var pos = Resume.get(e), meta = (pos ? 'Просмотрено до ' + fmtTime(pos) + ' · ' : '') + fmtSize(e.size);
      var m = t.querySelector('.meta');
      if (m) m.textContent = meta;
    }
  }
};

$('browser-switch').onActivate = function () { App.show(Servers); };
$('browser-scroll').addEventListener('scroll', function () { Browser.loadThumbs(); });
document.addEventListener('mousewheel', function (e) {
  if (App.screen === Browser) { $('browser-scroll').scrollTop += e.wheelDelta < 0 ? 200 : -200; }
});
