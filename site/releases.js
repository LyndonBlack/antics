// Fills the download buttons from the latest GitHub release. Without JavaScript (or if the request
// fails) the buttons simply link to the Releases page. One request to api.github.com, nothing else.
(function () {
  var status = document.getElementById('rel-status');
  var rules = {
    'win-setup': /-windows-x86_64-setup\.exe$/,
    'win-zip':   /-windows-x86_64\.zip$/,
    'mac-arm':   /-macos-aarch64\.tar\.gz$/,
    'mac-intel': /-macos-x86_64\.tar\.gz$/,
    'linux':     /-linux-x86_64\.tar\.gz$/
  };
  function mb(n) { return (n / 1048576).toFixed(1) + ' MB'; }
  fetch('https://api.github.com/repos/LyndonBlack/antics/releases/latest', { headers: { Accept: 'application/vnd.github+json' } })
    .then(function (r) { if (!r.ok) throw new Error(r.status); return r.json(); })
    .then(function (rel) {
      var found = 0;
      (rel.assets || []).forEach(function (a) {
        Object.keys(rules).forEach(function (key) {
          if (!rules[key].test(a.name)) return;
          var el = document.querySelector('[data-os="' + key + '"]');
          if (!el) return;
          el.href = a.browser_download_url;
          var s = document.createElement('span');
          s.className = 'size';
          s.textContent = a.name + ' · ' + mb(a.size);
          el.appendChild(s);
          found++;
        });
      });
      var v = (rel.tag_name || '').replace(/^v/, '');
      status.textContent = found ? 'Latest version: ' + v + '. Pick your system:' : 'Pick your system on the releases page:';
    })
    .catch(function () {
      status.textContent = 'Pick your system (the buttons open the releases page):';
    });
})();
