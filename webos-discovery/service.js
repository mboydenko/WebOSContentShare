/*
 * webOS JS service: web apps cannot send UDP, so the app asks this service to
 * broadcast a discovery query and collect the servers' replies.
 *
 *   luna://com.contentshare.player.discovery/discover {"timeout": 1500}
 *   -> {"returnValue": true, "servers": [{"address", "port", "name", "id"}]}
 *
 * ES5 only: webOS 3.x/4.x ship Node.js 0.12.
 */

var Service = require('webos-service');
var dgram = require('dgram');
var os = require('os');
var pkg = require('./package.json');

var PORT = 8201;
var QUERY = 'WEBOS-CONTENT-SHARE-DISCOVER';

var service = new Service(pkg.name);

function toBuffer(s) {
  return typeof Buffer.alloc === 'function' ? Buffer.from(s) : new Buffer(s);
}

/* 255.255.255.255 plus each interface's directed broadcast (some networks drop the former). */
function broadcastAddresses() {
  var out = ['255.255.255.255'], ifs = os.networkInterfaces();
  Object.keys(ifs).forEach(function (name) {
    ifs[name].forEach(function (a) {
      if ((a.family !== 'IPv4' && a.family !== 4) || a.internal) return;
      var ip = a.address.split('.'), mask = (a.netmask || '255.255.255.0').split('.');
      var b = ip.map(function (o, i) { return (Number(o) | (~Number(mask[i]) & 255)); }).join('.');
      if (out.indexOf(b) < 0) out.push(b);
    });
  });
  return out;
}

service.register('discover', function (message) {
  var wait = Math.min(Math.max(Number(message.payload.timeout) || 1500, 300), 5000);
  var servers = [], seen = {}, done = false, sock;

  function finish(warning) {
    if (done) return;
    done = true;
    try { sock.close(); } catch (e) {}
    message.respond({ returnValue: true, servers: servers, warning: warning });
  }

  try {
    sock = dgram.createSocket('udp4');
  } catch (e) {
    message.respond({ returnValue: false, errorText: String(e) });
    return;
  }

  sock.on('message', function (msg, rinfo) {
    var d;
    try { d = JSON.parse(msg.toString()); } catch (e) { return; }
    if (!d || d.app !== 'webos-content-share' || !d.port) return;
    var key = rinfo.address + ':' + d.port;
    if (seen[key]) return;
    seen[key] = true;
    servers.push({ address: rinfo.address, port: d.port, name: d.name || rinfo.address, id: d.id || '' });
  });
  sock.on('error', function (e) { finish(String(e)); });

  sock.bind(function () {
    try { sock.setBroadcast(true); } catch (e) {}
    var buf = toBuffer(QUERY), targets = broadcastAddresses();
    function send() {
      targets.forEach(function (addr) {
        sock.send(buf, 0, buf.length, PORT, addr, function () {});
      });
    }
    send();
    setTimeout(send, 400); // UDP can be lost: ask twice
    setTimeout(finish, wait);
  });
});
