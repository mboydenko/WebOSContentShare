/* Remote-control navigation: spatial focus between .focusable elements, dialogs, toasts. */

var KEY = {
  LEFT: 37, UP: 38, RIGHT: 39, DOWN: 40, ENTER: 13, SPACE: 32,
  BACK: 461, ESC: 27, BACKSPACE: 8,
  PLAY: 415, PAUSE: 19, STOP: 413, REW: 412, FF: 417,
  CH_UP: 33, CH_DOWN: 34
};

function isBackKey(code) { return code === KEY.BACK || code === KEY.ESC || code === KEY.BACKSPACE; }

var Nav = (function () {
  var scope = null, current = null;

  function visible(el) {
    if (!el.offsetParent) return false;
    var r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  }

  function list() {
    if (!scope) return [];
    var all = scope.querySelectorAll('.focusable'), out = [];
    for (var i = 0; i < all.length; i++) if (visible(all[i])) out.push(all[i]);
    return out;
  }

  function scrollerOf(el) {
    var p = el.parentNode;
    while (p && p.classList) {
      if (p.classList.contains('scroller')) return p;
      p = p.parentNode;
    }
    return null;
  }

  function ensureVisible(el) {
    var sc = scrollerOf(el);
    if (!sc) return;
    var r = el.getBoundingClientRect(), s = sc.getBoundingClientRect(), m = 50;
    if (r.top < s.top + m) sc.scrollTop -= (s.top + m - r.top);
    else if (r.bottom > s.bottom - m) sc.scrollTop += (r.bottom - (s.bottom - m));
  }

  function focus(el, noScroll) {
    if (current === el) return;
    if (current) {
      current.classList.remove('focused');
      if (current.tagName === 'INPUT') current.blur();
    }
    current = el || null;
    if (!current) return;
    current.classList.add('focused');
    if (!noScroll) ensureVisible(current);
    if (current.onNavFocus) current.onNavFocus();
  }

  function move(dir) {
    var items = list();
    if (!items.length) return false;
    if (!current || items.indexOf(current) < 0) { focus(items[0]); return true; }
    var c = current.getBoundingClientRect();
    var cx = c.left + c.width / 2, cy = c.top + c.height / 2;
    var best = null, bestScore = Infinity;
    for (var i = 0; i < items.length; i++) {
      var el = items[i];
      if (el === current) continue;
      var r = el.getBoundingClientRect();
      var x = r.left + r.width / 2, y = r.top + r.height / 2, main, cross;
      var overlapX = r.left < c.right && r.right > c.left;
      var overlapY = r.top < c.bottom && r.bottom > c.top;
      if (dir === 'right') { if (x <= cx + 1) continue; main = x - cx; cross = overlapY ? 0 : Math.abs(y - cy); }
      else if (dir === 'left') { if (x >= cx - 1) continue; main = cx - x; cross = overlapY ? 0 : Math.abs(y - cy); }
      else if (dir === 'down') { if (y <= cy + 1) continue; main = y - cy; cross = overlapX ? 0 : Math.abs(x - cx); }
      else { if (y >= cy - 1) continue; main = cy - y; cross = overlapX ? 0 : Math.abs(x - cx); }
      var score = main + cross * 3;
      if (score < bestScore) { bestScore = score; best = el; }
    }
    if (best) { focus(best); return true; }
    return false;
  }

  return {
    setScope: function (el, first) { scope = el; focus(first || list()[0] || null); },
    getScope: function () { return scope; },
    focus: focus,
    move: move,
    current: function () { return current; },
    ensureVisible: ensureVisible
  };
})();

function activate(el) {
  if (!el) return;
  if (el.tagName === 'INPUT') { el.focus(); return; }
  if (el.onActivate) el.onActivate();
}

function closestFocusable(el) {
  while (el && el.classList) {
    if (el.classList.contains('focusable')) return el;
    el = el.parentNode;
  }
  return null;
}

/* Modal dialog with buttons: [{label, action}]. onBack runs when Back is pressed. */
var Dialog = (function () {
  var open = false, prevScope = null, prevFocus = null, backAction = null;

  function close() {
    if (!open) return;
    open = false;
    $('dialog').classList.remove('show');
    Nav.setScope(prevScope, prevFocus);
  }

  return {
    isOpen: function () { return open; },
    open: function (text, buttons, onBack) {
      if (!open) { prevScope = Nav.getScope(); prevFocus = Nav.current(); }
      open = true;
      backAction = onBack || null;
      $('dialog-text').innerHTML = esc(text).replace(/\n/g, '<br>');
      var box = $('dialog-buttons');
      box.innerHTML = '';
      for (var i = 0; i < buttons.length; i++) {
        (function (b) {
          var el = document.createElement('div');
          el.className = 'btn focusable';
          el.textContent = b.label;
          el.onActivate = function () { close(); if (b.action) b.action(); };
          box.appendChild(el);
        })(buttons[i]);
      }
      $('dialog').classList.add('show');
      Nav.setScope($('dialog'));
    },
    close: close,
    onKey: function (code) {
      if (code === KEY.LEFT) Nav.move('left');
      else if (code === KEY.RIGHT) Nav.move('right');
      else if (code === KEY.ENTER) activate(Nav.current());
      else if (isBackKey(code)) { var a = backAction; close(); if (a) a(); }
      return true;
    }
  };
})();

function toast(msg, ms) {
  var t = $('toast');
  t.textContent = msg;
  t.classList.add('show');
  clearTimeout(toast.timer);
  toast.timer = setTimeout(function () { t.classList.remove('show'); }, ms || 3000);
}

function exitApp() {
  try {
    if (window.webOS && window.webOS.platformBack) { window.webOS.platformBack(); return; }
    if (window.PalmSystem && window.PalmSystem.platformBack) { window.PalmSystem.platformBack(); return; }
  } catch (e) {}
  window.close();
}
