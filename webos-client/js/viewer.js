/* Photo viewer: left/right to browse, OK toggles a slideshow. Images are downscaled by the server. */

var Viewer = {
  el: $('screen-viewer'),
  list: [],
  index: 0,
  slideshow: null,
  captionTimer: null,

  open: function (list, index) {
    Viewer.list = list;
    App.show(Viewer);
    Viewer.show(index < 0 ? 0 : index);
  },

  show: function (i) {
    var V = Viewer, item = V.list[i];
    if (!item) return;
    V.index = i;
    var img = $('viewer-img');
    $('viewer-spinner').className = 'spinner show';
    img.onload = function () { $('viewer-spinner').className = 'spinner'; };
    img.onerror = function () { $('viewer-spinner').className = 'spinner'; toast('Не удалось загрузить изображение'); };
    img.src = Api.thumbUrl(item.root, item.path, 1920);
    V.caption(item.name + '   ' + (i + 1) + ' / ' + V.list.length + (V.slideshow ? '   · слайд-шоу' : ''));
    var next = V.list[i + 1];
    if (next) new Image().src = Api.thumbUrl(next.root, next.path, 1920);
  },

  caption: function (text) {
    var c = $('viewer-caption');
    c.textContent = text;
    c.className = 'caption show';
    Viewer.el.classList.add('ui-visible');
    clearTimeout(Viewer.captionTimer);
    Viewer.captionTimer = setTimeout(function () {
      c.className = 'caption';
      Viewer.el.classList.remove('ui-visible');
    }, 3000);
  },

  step: function (d) {
    var n = Viewer.index + d;
    if (n >= Viewer.list.length) n = Viewer.slideshow ? 0 : Viewer.list.length - 1;
    if (n < 0) n = 0;
    if (n !== Viewer.index) Viewer.show(n);
  },

  toggleSlideshow: function () {
    if (Viewer.slideshow) {
      clearInterval(Viewer.slideshow);
      Viewer.slideshow = null;
      Viewer.caption('Слайд-шоу остановлено');
    } else {
      Viewer.slideshow = setInterval(function () { Viewer.step(1); }, 5000);
      Viewer.caption('Слайд-шоу: смена каждые 5 секунд');
    }
  },

  onHide: function () {
    clearInterval(Viewer.slideshow);
    Viewer.slideshow = null;
    $('viewer-img').removeAttribute('src');
  },

  onKey: function (code) {
    if (code === KEY.LEFT || code === KEY.CH_DOWN) Viewer.step(-1);
    else if (code === KEY.RIGHT || code === KEY.CH_UP) Viewer.step(1);
    else if (code === KEY.ENTER || code === KEY.PLAY || code === KEY.PAUSE) Viewer.toggleSlideshow();
    else if (code === KEY.UP || code === KEY.DOWN) Viewer.show(Viewer.index);
    else if (isBackKey(code) || code === KEY.STOP) App.show(Browser);
    else return false;
    return true;
  }
};

Viewer.el.addEventListener('click', function (e) {
  if (closestFocusable(e.target)) return; // the Back button
  Viewer.step(e.clientX < window.innerWidth / 2 ? -1 : 1);
});
Viewer.el.addEventListener('mousemove', function () {
  if (App.screen === Viewer) Viewer.caption($('viewer-caption').textContent);
});
