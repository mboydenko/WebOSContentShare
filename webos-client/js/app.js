/* Screen switching, global key handling and startup. */

var App = {
  screen: null,
  serverName: '',

  show: function (scr) {
    if (App.screen === scr) return;
    if (App.screen && App.screen.onHide) App.screen.onHide();
    var all = document.querySelectorAll('.screen');
    for (var i = 0; i < all.length; i++) all[i].classList.remove('active');
    scr.el.classList.add('active');
    App.screen = scr;
    if (scr.onShow) scr.onShow();
    else Nav.setScope(scr.el);
  },

  connect: function (base) {
    toast('Подключение к ' + base.replace(/^https?:\/\//, '') + '…', 2000);
    probeServer(base, function (s) {
      if (!s) {
        toast('Сервер ' + base.replace(/^https?:\/\//, '') + ' не отвечает', 4000);
        if (App.screen !== Servers) App.show(Servers);
        return;
      }
      Api.base = s.base;
      App.serverName = s.name;
      Store.set('server', s.base);
      Browser.home();
    }, 5000);
  }
};

document.addEventListener('keydown', function (e) {
  var code = e.keyCode;
  if (Dialog.isOpen()) { Dialog.onKey(code); e.preventDefault(); return; }

  var inInput = document.activeElement && document.activeElement.tagName === 'INPUT';
  if (inInput && (code === KEY.LEFT || code === KEY.RIGHT || code === KEY.BACKSPACE || (code >= 48 && code <= 90) || code === KEY.SPACE)) {
    return; // typing / caret movement inside the field
  }
  if (inInput && (code === KEY.UP || code === KEY.DOWN)) document.activeElement.blur();

  if (App.screen && App.screen.onKey && App.screen.onKey(code, e)) { e.preventDefault(); return; }

  switch (code) {
    case KEY.LEFT: Nav.move('left'); break;
    case KEY.RIGHT: Nav.move('right'); break;
    case KEY.UP: Nav.move('up'); break;
    case KEY.DOWN: Nav.move('down'); break;
    case KEY.ENTER: activate(Nav.current()); break;
    default: return;
  }
  e.preventDefault();
}, true);

/* Magic Remote pointer / mouse: hover focuses, click activates. */
document.addEventListener('mouseover', function (e) {
  var el = closestFocusable(e.target);
  if (el && el.tagName !== 'INPUT') Nav.focus(el, true);
});
document.addEventListener('click', function (e) {
  var el = closestFocusable(e.target);
  if (el) { Nav.focus(el, true); activate(el); }
});

/* Pause when the app goes to the background. */
function onVisibility() {
  var hidden = document.hidden || document.webkitHidden;
  if (hidden && App.screen === Player && !Player.video.paused) Player.video.pause();
}
document.addEventListener('visibilitychange', onVisibility);
document.addEventListener('webkitvisibilitychange', onVisibility);

(function start() {
  Player.init();
  // Opened from the server itself (http://pc:8200/app/): no discovery needed.
  if (/^https?:$/.test(location.protocol) && location.pathname.indexOf('/app') === 0) {
    App.connect(location.protocol + '//' + location.host);
    return;
  }
  var saved = Store.get('server');
  if (!saved) { App.show(Servers); return; }
  probeServer(saved, function (s) {
    if (s) {
      Api.base = s.base;
      App.serverName = s.name;
      Browser.home();
    } else {
      App.show(Servers);
      toast('Сохранённый сервер не отвечает — ищу серверы в сети', 4000);
    }
  }, 4000);
})();
