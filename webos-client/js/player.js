/*
 * Video/audio player.
 *
 * Direct mode: <video> plays the original file (HTTP Range), seeking is native.
 * Stream mode: the server remuxes/transcodes into HLS starting at `offset`
 * seconds; seeking restarts the stream, so position = offset + currentTime.
 * If direct play fails the player falls back to streaming, and a failed remux
 * falls back to a full transcode.
 */

function parseVttTime(s) {
  var p = s.replace(',', '.').split(':');
  var sec = parseFloat(p.pop()) || 0, m = parseInt(p.pop() || '0', 10), h = parseInt(p.pop() || '0', 10);
  return h * 3600 + m * 60 + sec;
}

function cueHtml(text) {
  return esc(text.replace(/\{\\[^}]*\}/g, ''))
    .replace(/&lt;(\/?)(i|b|u)&gt;/g, '<$1$2>')
    .replace(/&lt;[^&]*&gt;/g, '')
    .replace(/\\N/g, '<br>')
    .replace(/\n/g, '<br>');
}

function parseVtt(text) {
  var cues = [], blocks = text.replace(/\r/g, '').split(/\n\s*\n/);
  var re = /((?:\d+:)?\d+:\d+[.,]\d+)\s*-->\s*((?:\d+:)?\d+:\d+[.,]\d+)/;
  for (var i = 0; i < blocks.length; i++) {
    var lines = blocks[i].split('\n');
    for (var j = 0; j < lines.length; j++) {
      var m = lines[j].match(re);
      if (m) {
        var body = lines.slice(j + 1).join('\n');
        if (body) cues.push({ s: parseVttTime(m[1]), e: parseVttTime(m[2]), html: cueHtml(body) });
        break;
      }
    }
  }
  cues.sort(function (a, b) { return a.s - b.s; });
  return cues;
}

var Player = {
  el: $('screen-player'),
  video: $('video'),
  gen: 0,          // bumps on every source change; stale callbacks compare against it
  srcGen: -1,

  reset: function () {
    var P = Player;
    P.item = null; P.list = []; P.info = null;
    P.mode = null; P.streamMode = null; P.encoder = null; P.session = null; P.offset = 0;
    P.audioIndex = null; P.subId = null; P.burn = null; P.cues = null;
    P.forceStream = false; P.forceTranscode = false; P.retries = 0;
    P.pendingSeek = 0; P.seekTarget = null; P.playingOnce = false; P.panelOpen = false;
    P.lastSeekKey = 0; P.seekPresses = 0;
    clearTimeout(P.seekTimer); clearTimeout(P.watchdog); clearTimeout(P.osdTimer); clearTimeout(P.videoCheck);
    $('subtitle').innerHTML = '';
    $('tracks').classList.remove('show');
    $('audio-art').classList.remove('show');
  },

  init: function () {
    var P = Player, v = P.video;
    v.addEventListener('loadedmetadata', function () {
      if (P.srcGen !== P.gen) return;
      if (P.pendingSeek > 0) { try { v.currentTime = P.pendingSeek; } catch (e) {} P.pendingSeek = 0; }
    });
    v.addEventListener('playing', function () {
      if (P.srcGen !== P.gen) return;
      P.spinner(false);
      clearTimeout(P.watchdog);
      if (!P.playingOnce) { P.playingOnce = true; P.showOsd(); P.checkVideoDecodes(); }
      P.updateState();
    });
    v.addEventListener('waiting', function () { if (P.srcGen === P.gen) P.spinner(true); });
    v.addEventListener('pause', function () { P.updateState(); P.showOsd(); });
    v.addEventListener('play', function () { P.updateState(); });
    v.addEventListener('timeupdate', function () { if (P.srcGen === P.gen) { P.updateOsd(); P.renderSubs(); } });
    v.addEventListener('ended', function () { if (P.srcGen === P.gen) P.onEnded(); });
    v.addEventListener('error', function () { P.onError(); });

    setInterval(function () {
      if (P.session) Api.get('/api/stream/' + P.session + '/keepalive', function () {});
    }, 15000);
    setInterval(function () {
      if (P.item && P.info && P.info.kind === 'video' && P.playingOnce && !P.video.paused) {
        Resume.set(P.item, P.position(), P.info.duration);
      }
    }, 10000);
    setInterval(function () { if (App.screen === P) P.renderSubs(); }, 250);
  },

  open: function (item, list) {
    var P = Player;
    P.stopSession();
    P.reset();
    P.item = item;
    P.list = list || [item];
    var g = ++P.gen;
    App.show(P);
    P.spinner(true);
    $('osd-title').textContent = item.name;
    $('osd-badges').innerHTML = '';
    P.updateOsd();
    P.showOsd();

    Api.get('/api/media?' + qs({ root: item.root, path: item.path }), function (err, info) {
      if (g !== P.gen) return;
      if (err) { P.fail('Не удалось открыть файл.\n' + err.message); return; }
      P.info = info;
      P.audioIndex = info.defaultAudio;
      if (info.kind === 'audio') P.showAudioArt();
      var saved = info.kind === 'video' ? Resume.get(item) : 0;
      if (saved && info.duration && saved < info.duration - 10) {
        P.spinner(false);
        Dialog.open('Продолжить просмотр с ' + fmtTime(saved) + '?', [
          { label: 'Продолжить', action: function () { P.startAt(saved); } },
          { label: 'С начала', action: function () { P.startAt(0); } }
        ], function () { P.close(); });
      } else {
        P.startAt(0);
      }
    });
  },

  showAudioArt: function () {
    var it = Player.item;
    $('audio-title').textContent = it.name.replace(/\.[^.]+$/, '');
    $('audio-sub').textContent = Player.info.audio[0] ? Player.info.audio[0].label : '';
    var img = $('audio-cover');
    img.style.visibility = 'hidden';
    img.onload = function () { img.style.visibility = 'visible'; };
    img.src = Api.thumbUrl(it.root, it.path, 480);
    $('audio-art').classList.add('show');
  },

  startAt: function (t) {
    var P = Player, info = P.info;
    if (info.direct && !P.forceStream && P.burn === null && P.audioIndex === info.defaultAudio) P.playDirect(t);
    else P.startStream(t);
  },

  restart: function () { Player.startAt(Player.position()); },

  playDirect: function (t) {
    var P = Player, g = ++P.gen;
    P.stopSession();
    P.mode = 'direct'; P.streamMode = 'direct'; P.offset = 0; P.playingOnce = false;
    P.pendingSeek = t > 0 ? t : 0;
    P.renderBadges();
    P.spinner(true);
    P.setSource(Api.rawUrl(P.item.root, P.item.path), null);
    P.armWatchdog(g, 20000);
  },

  startStream: function (t) {
    var P = Player, g = ++P.gen;
    P.stopSession();
    P.clearSource();
    P.spinner(true);
    P.mode = 'stream'; P.offset = t; P.pendingSeek = 0; P.playingOnce = false;
    P.updateOsd();
    var params = {
      root: P.item.root, path: P.item.path, start: t.toFixed(2), client: clientId(),
      mode: P.forceTranscode ? 'transcode' : 'auto', audio: P.audioIndex, burn: P.burn
    };
    Api.get('/api/stream/start?' + qs(params), function (err, r) {
      if (g !== P.gen) return;
      if (err) { P.fail('Не удалось запустить трансляцию.\n' + err.message); return; }
      P.session = r.id;
      P.offset = r.start;
      P.streamMode = r.mode;
      P.encoder = r.encoder;
      P.renderBadges();
      P.setSource(Api.base + r.url, 'application/vnd.apple.mpegurl');
      P.armWatchdog(g, 40000);
    }, 180000);
  },

  setSource: function (url, type) {
    var P = Player, v = P.video;
    P.srcGen = P.gen;
    v.pause();
    while (v.firstChild) v.removeChild(v.firstChild);
    v.removeAttribute('src');
    if (type) {
      var s = document.createElement('source');
      s.src = url;
      s.type = type;
      s.addEventListener('error', function () { P.onError(); });
      v.appendChild(s);
    } else {
      v.src = url;
    }
    v.load();
    P.play();
  },

  clearSource: function () {
    var v = Player.video;
    Player.srcGen = -1;
    v.pause();
    while (v.firstChild) v.removeChild(v.firstChild);
    v.removeAttribute('src');
    try { v.load(); } catch (e) {}
  },

  play: function () {
    var r = Player.video.play();
    if (r && r['catch']) r['catch'](function () {});
  },

  armWatchdog: function (g, ms) {
    clearTimeout(Player.watchdog);
    Player.watchdog = setTimeout(function () {
      if (g === Player.gen && !Player.playingOnce) Player.fallback('Воспроизведение не началось');
    }, ms);
  },

  /* Some TVs play the audio of an unsupported video codec with a black picture. */
  checkVideoDecodes: function () {
    var P = Player, g = P.gen;
    if (P.mode !== 'direct' || !P.info || !P.info.video) return;
    clearTimeout(P.videoCheck);
    P.videoCheck = setTimeout(function () {
      if (g === P.gen && P.video.videoWidth === 0) P.fallback('ТВ не декодирует видео');
    }, 2500);
  },

  onError: function () {
    var P = Player;
    if (P.srcGen !== P.gen || !P.info) return;
    var pos = P.position();
    if (P.playingOnce && P.info.duration && pos > P.info.duration - 15) { P.onEnded(); return; }
    P.fallback('Ошибка воспроизведения');
  },

  fallback: function (why) {
    var P = Player, pos = P.position();
    clearTimeout(P.watchdog);
    if (P.mode === 'direct') {
      P.forceStream = true;
      toast(why + ' — включаю потоковый режим');
      P.startStream(pos);
    } else if (P.streamMode === 'remux' && !P.forceTranscode) {
      P.forceTranscode = true;
      toast(why + ' — перекодирую видео');
      P.startStream(pos);
    } else if (P.retries < 1) {
      P.retries++;
      P.startStream(pos);
    } else {
      P.fail(why + '.');
    }
  },

  fail: function (msg) {
    var P = Player;
    P.gen++;
    P.spinner(false);
    P.stopSession();
    P.clearSource();
    Dialog.open(msg, [
      { label: 'Повторить', action: function () { if (P.item) P.open(P.item, P.list); } },
      { label: 'Назад', action: function () { P.close(); } }
    ], function () { P.close(); });
  },

  stopSession: function () {
    if (Player.session) {
      Api.get('/api/stream/' + Player.session + '/stop', function () {});
      Player.session = null;
    }
  },

  position: function () {
    var t = Player.video.currentTime;
    return Player.offset + (isFinite(t) ? t : 0);
  },

  duration: function () {
    if (Player.info && Player.info.duration) return Player.info.duration;
    var d = Player.video.duration;
    return isFinite(d) ? d : 0;
  },

  close: function () {
    var P = Player;
    if (P.item && P.info && P.info.kind === 'video' && P.playingOnce) Resume.set(P.item, P.position(), P.info.duration);
    P.gen++;
    P.stopSession();
    P.clearSource();
    P.reset();
    P.spinner(false);
    App.show(Browser);
    Browser.refreshMeta();
  },

  onEnded: function () {
    var P = Player;
    if (P.info && P.info.kind === 'video') Resume.set(P.item, 0, 0);
    if (P.info && P.info.kind === 'audio' && P.next(1)) return;
    P.close();
  },

  next: function (dir) {
    var P = Player, i = P.list.indexOf(P.item) + dir;
    if (i < 0 || i >= P.list.length) return false;
    if (P.info && P.info.kind === 'video' && P.playingOnce) Resume.set(P.item, P.position(), P.info.duration);
    P.open(P.list[i], P.list);
    return true;
  },

  /* Seeking: presses accumulate into a target, applied after a short pause. */
  seekBy: function (delta) {
    var P = Player;
    if (!P.info || !P.mode) return;
    var base = P.seekTarget !== null ? P.seekTarget : P.position(), dur = P.duration();
    P.seekTarget = Math.max(0, dur ? Math.min(dur - 3, base + delta) : base + delta);
    P.showOsd();
    P.updateOsd();
    clearTimeout(P.seekTimer);
    P.seekTimer = setTimeout(function () {
      var t = P.seekTarget;
      P.seekTarget = null;
      P.seekTo(t);
    }, 900);
  },

  seekStep: function () {
    var P = Player, now = Date.now();
    P.seekPresses = now - P.lastSeekKey < 700 ? P.seekPresses + 1 : 0;
    P.lastSeekKey = now;
    return P.seekPresses < 4 ? 10 : P.seekPresses < 10 ? 30 : 60;
  },

  seekTo: function (t) {
    var P = Player;
    if (P.mode === 'direct') {
      try { P.video.currentTime = t; } catch (e) {}
      P.play();
    } else {
      P.startStream(t);
    }
    P.updateOsd();
  },

  togglePause: function () {
    var v = Player.video;
    if (v.paused) Player.play(); else v.pause();
    Player.showOsd();
  },

  /* Track panel */
  openPanel: function () {
    var P = Player, info = P.info;
    if (!info) return;
    var a = $('tracks-audio'), s = $('tracks-subs'), first = null;
    a.innerHTML = ''; s.innerHTML = '';
    function add(box, label, selected, action) {
      var el = document.createElement('div');
      el.className = 'track focusable' + (selected ? ' selected' : '');
      el.textContent = label;
      el.onActivate = function () { P.closePanel(); action(); };
      box.appendChild(el);
      if (selected && !first) first = el;
      return el;
    }
    for (var i = 0; i < info.audio.length; i++) {
      (function (tr) {
        add(a, tr.label || ('Дорожка ' + tr.index), tr.index === P.audioIndex, function () { P.selectAudio(tr.index); });
      })(info.audio[i]);
    }
    if (!info.audio.length) a.innerHTML = '<div class="hint">Нет</div>';
    if (info.kind === 'video') {
      add(s, 'Выключены', P.subId === null, function () { P.selectSub(null); });
      for (var j = 0; j < info.subtitles.length; j++) {
        (function (sub) {
          add(s, sub.label + (sub.text ? '' : ' (вшиваются)'), sub.id === P.subId, function () { P.selectSub(sub); });
        })(info.subtitles[j]);
      }
    }
    P.panelOpen = true;
    $('tracks').classList.add('show');
    Nav.setScope($('tracks'), first);
  },

  closePanel: function () {
    Player.panelOpen = false;
    $('tracks').classList.remove('show');
    Nav.setScope(Player.el);
  },

  selectAudio: function (index) {
    var P = Player;
    if (index === P.audioIndex) return;
    P.audioIndex = index;
    if (P.mode === 'direct') {
      // Native track switching avoids a remux when the TV exposes audioTracks.
      var tracks = P.video.audioTracks, pos = -1;
      for (var i = 0; i < P.info.audio.length; i++) if (P.info.audio[i].index === index) pos = i;
      if (tracks && tracks.length === P.info.audio.length && pos >= 0) {
        for (var k = 0; k < tracks.length; k++) tracks[k].enabled = k === pos;
        toast('Звуковая дорожка переключена');
        return;
      }
    }
    toast('Переключаю звуковую дорожку…');
    P.restart();
  },

  selectSub: function (sub) {
    var P = Player, hadBurn = P.burn !== null;
    P.cues = null;
    $('subtitle').innerHTML = '';
    if (!sub) {
      P.subId = null; P.burn = null;
      if (hadBurn) P.restart();
      return;
    }
    P.subId = sub.id;
    if (sub.text) {
      P.burn = null;
      if (hadBurn) P.restart();
      toast('Загрузка субтитров…', 1500);
      var id = sub.id, g = P.gen;
      Api.getText('/api/subs?' + qs({ root: P.item.root, path: P.item.path, id: id }), function (err, text) {
        if (P.subId !== id || !P.item) return;
        if (err) { toast('Субтитры недоступны: ' + err.message); return; }
        P.cues = parseVtt(text);
        if (!P.cues.length) toast('Субтитры пусты');
      }, 900000);
    } else {
      P.burn = sub.index;
      toast('Графические субтитры будут вшиты в видео');
      P.restart();
    }
  },

  renderSubs: function () {
    var P = Player, el = $('subtitle');
    if (!P.cues) { if (el.innerHTML) el.innerHTML = ''; return; }
    var t = P.position(), out = [];
    for (var i = 0; i < P.cues.length; i++) {
      var c = P.cues[i];
      if (c.s > t) break;
      if (c.e > t) out.push(c.html);
    }
    var html = out.join('<br>');
    if (el.innerHTML !== html) el.innerHTML = html;
  },

  /* OSD */
  showOsd: function () {
    var P = Player;
    P.el.classList.add('osd-visible');
    clearTimeout(P.osdTimer);
    P.osdTimer = setTimeout(function () {
      if (!P.video.paused && P.seekTarget === null && !P.panelOpen) P.el.classList.remove('osd-visible');
    }, 4000);
    P.updateOsd();
  },

  updateState: function () {
    $('osd-state').textContent = Player.video.paused ? '❚❚' : '▶';
  },

  updateOsd: function () {
    var P = Player, dur = P.duration(), pos = P.position();
    $('osd-time').textContent = fmtTime(pos);
    $('osd-dur').textContent = dur ? fmtTime(dur) : '';
    $('osd-bar').style.width = dur ? Math.min(100, pos / dur * 100) + '%' : '0';
    var mark = $('osd-seek');
    if (P.seekTarget !== null && dur) {
      mark.style.display = 'block';
      mark.style.left = (P.seekTarget / dur * 100) + '%';
      $('osd-time').textContent = '→ ' + fmtTime(P.seekTarget);
    } else {
      mark.style.display = 'none';
    }
  },

  renderBadges: function () {
    var P = Player, info = P.info, b = [];
    if (!info) return;
    var v = info.video;
    if (v) {
      var res = v.height >= 2000 ? '4K' : v.height >= 1400 ? '1440p' : v.height >= 1000 ? '1080p' : v.height >= 700 ? '720p' : v.height + 'p';
      b.push('<span>' + res + '</span>');
      if (v.hdr) b.push('<span>' + esc(v.hdr) + '</span>');
    }
    var modes = { direct: 'Оригинал', remux: 'Оригинальное видео', transcode: 'Перекодирование', audio: 'Поток' };
    var label = modes[P.streamMode] || '';
    if (P.streamMode === 'transcode' && P.encoder) label += ' · ' + P.encoder.replace(/^(h264|hevc|lib)_?/, '').toUpperCase();
    if (label) b.push('<span class="accent">' + esc(label) + '</span>');
    $('osd-badges').innerHTML = b.join('');
  },

  spinner: function (on) { $('spinner').className = on ? 'spinner show' : 'spinner'; },

  onKey: function (code) {
    var P = Player;
    if (P.panelOpen) {
      if (isBackKey(code)) P.closePanel();
      else if (code === KEY.UP) Nav.move('up');
      else if (code === KEY.DOWN) Nav.move('down');
      else if (code === KEY.LEFT) Nav.move('left');
      else if (code === KEY.RIGHT) Nav.move('right');
      else if (code === KEY.ENTER) activate(Nav.current());
      return true;
    }
    switch (code) {
      case KEY.ENTER: case KEY.SPACE: P.togglePause(); break;
      case KEY.PLAY: P.play(); P.showOsd(); break;
      case KEY.PAUSE: P.video.pause(); break;
      case KEY.LEFT: P.seekBy(-P.seekStep()); break;
      case KEY.RIGHT: P.seekBy(P.seekStep()); break;
      case KEY.REW: P.seekBy(-60); break;
      case KEY.FF: P.seekBy(60); break;
      case KEY.UP: P.showOsd(); break;
      case KEY.DOWN: P.openPanel(); break;
      case KEY.CH_UP: if (!P.next(1)) toast('Это последний файл в папке'); break;
      case KEY.CH_DOWN: if (!P.next(-1)) toast('Это первый файл в папке'); break;
      case KEY.STOP: P.close(); break;
      default:
        if (isBackKey(code)) P.close();
        else return false;
    }
    return true;
  }
};

Player.el.addEventListener('click', function (e) {
  if (!Player.panelOpen && !closestFocusable(e.target)) Player.togglePause();
});
Player.el.addEventListener('mousemove', function () { if (App.screen === Player) Player.showOsd(); });
