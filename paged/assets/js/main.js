(function() {
'use strict';

/* === Theme System (3 themes) === */
var THEMES = ['light', 'dark', 'enflame'];

function getPreferredTheme() {
  var stored = localStorage.getItem('theme');
  if (stored && THEMES.indexOf(stored) !== -1) return stored;
  return 'light';
}

function setTheme(theme) {
  document.documentElement.setAttribute('data-theme', theme);
  localStorage.setItem('theme', theme);
  var sel = document.querySelector('.theme-select');
  if (sel) sel.value = theme;
}

setTheme(getPreferredTheme());

/* === Language System === */
function getLang() {
  return localStorage.getItem('xbot-lang') || 'en';
}

function setLang(lang) {
  localStorage.setItem('xbot-lang', lang);
  document.querySelectorAll('[data-zh]').forEach(function(el) {
    var text = lang === 'zh' ? el.getAttribute('data-zh') : el.getAttribute('data-en');
    if (text) el.textContent = text;
  });
  var btn = document.querySelector('.lang-toggle');
  if (btn) btn.textContent = lang === 'zh' ? 'EN' : '中文';
}

/* === DOMContentLoaded === */
document.addEventListener('DOMContentLoaded', function() {
  /* Theme dropdown */
  var sel = document.querySelector('.theme-select');
  if (sel) {
    sel.value = getPreferredTheme();
    sel.addEventListener('change', function() { setTheme(sel.value); });
  }

  /* Language toggle */
  var langBtn = document.querySelector('.lang-toggle');
  if (langBtn) {
    langBtn.addEventListener('click', function() {
      var cur = getLang();
      setLang(cur === 'zh' ? 'en' : 'zh');
    });
  }
  setLang(getLang());

  /* Scroll progress */
  var scrollBar = document.querySelector('.scroll-progress');
  if (scrollBar) {
    window.addEventListener('scroll', function() {
      var h = document.documentElement.scrollHeight - window.innerHeight;
      var pct = h > 0 ? (window.scrollY / h) * 100 : 0;
      scrollBar.style.width = pct + '%';
    }, { passive: true });
  }

  /* Header scroll */
  var header = document.querySelector('.site-header');
  if (header) {
    window.addEventListener('scroll', function() {
      header.classList.toggle('scrolled', window.scrollY > 10);
    }, { passive: true });
  }

  /* Reveal on scroll */
  var revealIO = new IntersectionObserver(function(entries) {
    entries.forEach(function(e) {
      if (e.isIntersecting) {
        e.target.classList.add('visible');
        revealIO.unobserve(e.target);
      }
    });
  }, { threshold: 0.08 });
  document.querySelectorAll('.reveal').forEach(function(el) { revealIO.observe(el); });

  /* Counter animation */
  var counted = new Set();
  function animateCounter(el) {
    if (counted.has(el)) return;
    counted.add(el);
    var target = parseFloat(el.getAttribute('data-target'));
    var dec = target % 1 ? 1 : 0;
    var duration = 1200;
    var start = performance.now();
    function tick(now) {
      var p = Math.min((now - start) / duration, 1);
      var ease = 1 - Math.pow(1 - p, 3);
      el.textContent = (target * ease).toFixed(dec);
      if (p < 1) requestAnimationFrame(tick);
      else el.textContent = target.toFixed(dec);
    }
    requestAnimationFrame(tick);
  }
  var counterIO = new IntersectionObserver(function(entries) {
    entries.forEach(function(e) {
      if (e.isIntersecting) {
        e.target.querySelectorAll('[data-counter]').forEach(animateCounter);
        counterIO.unobserve(e.target);
      }
    });
  }, { threshold: 0.25 });
  document.querySelectorAll('.stats-row').forEach(function(el) { counterIO.observe(el); });

  /* Tabs */
  document.querySelectorAll('.tab-btn').forEach(function(btn) {
    btn.addEventListener('click', function() {
      var tabGroup = btn.closest('.tabs');
      var contentParent = tabGroup ? tabGroup.parentElement : document;
      tabGroup.querySelectorAll('.tab-btn').forEach(function(b) { b.classList.remove('active'); });
      contentParent.querySelectorAll('.tab-content').forEach(function(c) { c.classList.remove('active'); });
      btn.classList.add('active');
      var panel = contentParent.querySelector('#' + btn.getAttribute('data-tab'));
      if (panel) panel.classList.add('active');
    });
  });

  /* Copy code */
  document.querySelectorAll('.copy-btn').forEach(function(btn) {
    btn.addEventListener('click', function() {
      var text = btn.getAttribute('data-copy-text');
      if (!text) {
        var block = btn.closest('.code-block');
        var code = block ? block.querySelector('pre code') : (btn.closest('pre') ? btn.closest('pre').querySelector('code') : null);
        text = code ? code.textContent : '';
      }
      navigator.clipboard.writeText(text.trim()).then(function() {
        btn.textContent = getLang() === 'zh' ? '已复制!' : 'Copied!';
        btn.classList.add('copied');
        setTimeout(function() {
          btn.textContent = getLang() === 'zh' ? '复制' : 'Copy';
          btn.classList.remove('copied');
        }, 2000);
      });
    });
  });

  /* Sidebar tracking */
  var sidebarLinks = document.querySelectorAll('.doc-sidebar a[href^="#"]');
  if (sidebarLinks.length > 0) {
    var sections = [];
    sidebarLinks.forEach(function(link) {
      var id = link.getAttribute('href').slice(1);
      var sec = document.getElementById(id);
      if (sec) sections.push({ el: sec, link: link });
    });
    window.addEventListener('scroll', function() {
      var scrollPos = window.scrollY + 120;
      var current = null;
      sections.forEach(function(s) {
        if (s.el.offsetTop <= scrollPos) current = s.link;
      });
      sidebarLinks.forEach(function(l) { l.classList.remove('active'); });
      if (current) current.classList.add('active');
    }, { passive: true });
  }

  /* Search overlay */
  var searchOverlay = document.querySelector('.search-overlay');
  if (searchOverlay) {
    var searchInput = searchOverlay.querySelector('.search-input');
    var searchResults = searchOverlay.querySelector('.search-results');

    document.querySelectorAll('[data-search-trigger]').forEach(function(trigger) {
      trigger.addEventListener('click', function() { searchOverlay.classList.add('active'); searchInput.focus(); });
    });
    searchOverlay.addEventListener('click', function(e) {
      if (e.target === searchOverlay) searchOverlay.classList.remove('active');
    });
    document.addEventListener('keydown', function(e) {
      if ((e.ctrlKey || e.metaKey) && e.key === 'k') { e.preventDefault(); searchOverlay.classList.toggle('active'); if (searchOverlay.classList.contains('active')) searchInput.focus(); }
      if (e.key === 'Escape') searchOverlay.classList.remove('active');
    });

    var SEARCH_INDEX = window.SEARCH_INDEX || [];
    searchInput.addEventListener('input', function() {
      var q = searchInput.value.toLowerCase().trim();
      if (!q) { searchResults.innerHTML = '<div class="search-hint">Type to search documentation...</div>'; return; }
      var results = SEARCH_INDEX.filter(function(item) {
        return item.title.toLowerCase().indexOf(q) !== -1 || item.desc.toLowerCase().indexOf(q) !== -1 || (item.keywords || '').toLowerCase().indexOf(q) !== -1;
      });
      if (results.length === 0) { searchResults.innerHTML = '<div class="search-hint">No results found.</div>'; return; }
      searchResults.innerHTML = results.map(function(r) {
        return '<a href="' + r.url + '" class="search-result-item"><h4>' + r.title + '</h4><p>' + r.desc + '</p></a>';
      }).join('');
    });
  }

  /* Particles */
  var canvas = document.getElementById('particles');
  if (canvas) {
    var ctx = canvas.getContext('2d');
    var particles = [];
    var mouse = { x: -1000, y: -1000 };

    function resizeCanvas() { canvas.width = window.innerWidth; canvas.height = window.innerHeight; }
    function getParticleColor() {
      var theme = document.documentElement.getAttribute('data-theme');
      if (theme === 'dark') return [129, 140, 248];
      if (theme === 'enflame') return [249, 115, 22];
      return [99, 102, 241];
    }

    function initParticles() {
      particles = [];
      for (var i = 0; i < 50; i++) {
        particles.push({
          x: Math.random() * canvas.width,
          y: Math.random() * canvas.height,
          r: Math.random() * 1.5 + 0.5,
          dx: (Math.random() - 0.5) * 0.3,
          dy: (Math.random() - 0.5) * 0.3,
          o: Math.random() * 0.4 + 0.1
        });
      }
    }

    function drawParticles() {
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      var rgb = getParticleColor();
      particles.forEach(function(p, i) {
        var dx = mouse.x - p.x, dy = mouse.y - p.y;
        var dist = Math.sqrt(dx * dx + dy * dy);
        if (dist < 150) { p.x += dx * 0.005; p.y += dy * 0.005; }

        ctx.beginPath();
        ctx.arc(p.x, p.y, p.r, 0, Math.PI * 2);
        ctx.fillStyle = 'rgba(' + rgb[0] + ',' + rgb[1] + ',' + rgb[2] + ',' + p.o + ')';
        ctx.fill();

        for (var j = i + 1; j < particles.length; j++) {
          var p2 = particles[j];
          var d = Math.sqrt(Math.pow(p.x - p2.x, 2) + Math.pow(p.y - p2.y, 2));
          if (d < 130) {
            ctx.beginPath();
            ctx.moveTo(p.x, p.y);
            ctx.lineTo(p2.x, p2.y);
            ctx.strokeStyle = 'rgba(' + rgb[0] + ',' + rgb[1] + ',' + rgb[2] + ',' + (0.1 * (1 - d / 130)) + ')';
            ctx.stroke();
          }
        }

        p.x += p.dx; p.y += p.dy;
        if (p.x < 0) p.x = canvas.width;
        if (p.x > canvas.width) p.x = 0;
        if (p.y < 0) p.y = canvas.height;
        if (p.y > canvas.height) p.y = 0;
      });
      requestAnimationFrame(drawParticles);
    }

    resizeCanvas(); initParticles(); drawParticles();
    window.addEventListener('resize', function() { resizeCanvas(); initParticles(); });
    document.addEventListener('mousemove', function(e) { mouse.x = e.clientX; mouse.y = e.clientY; });
  }

  /* Feature card mouse tracking */
  document.querySelectorAll('.feature-card, .community-card').forEach(function(card) {
    card.addEventListener('mousemove', function(e) {
      var rect = card.getBoundingClientRect();
      var x = ((e.clientX - rect.left) / rect.width) * 100;
      var y = ((e.clientY - rect.top) / rect.height) * 100;
      card.style.setProperty('--mouse-x', x + '%');
      card.style.setProperty('--mouse-y', y + '%');
    });
  });

  /* Hero typing animation */
  var typingContainer = document.getElementById('hero-typing-code');
  if (typingContainer) {
    var codeEl = typingContainer.querySelector('.code-block pre code') || typingContainer.querySelector('pre code') || typingContainer.querySelector('pre');
    var lines = [
      { text: '$ xbot onboard', html: '<span class="token-flag">$</span> <span class="token-keyword">xbot</span> onboard' },
      { text: '  Config: ~/.xbot/config.json  ✓', html: '  Config: <span class="token-string">~/.xbot/config.json</span>  <span class="token-value">✓</span>' },
      { text: '  Workspace: ~/.xbot/workspace  ✓', html: '  Workspace: <span class="token-string">~/.xbot/workspace</span>  <span class="token-value">✓</span>' },
      { text: '', html: '' },
      { text: '$ xbot repl', html: '<span class="token-flag">$</span> <span class="token-keyword">xbot</span> repl' },
      { text: '  xbot v0.1.8 · 26 providers · 12 tools · 18 skills', html: '  <span class="token-comment">xbot v0.1.8</span> · <span class="token-func">26</span> providers · <span class="token-func">12</span> tools · <span class="token-func">18</span> skills' },
      { text: '', html: '' },
      { text: '  > refactor auth module with tests', html: '  <span class="token-flag">&gt;</span> <span class="token-string">refactor auth module with tests</span>' },
      { text: '  Spawning 3 subagents...', html: '  Spawning <span class="token-func">3</span> subagents...' },
      { text: '  ✓ Refactored 4 files, added 12 tests', html: '  <span class="token-value">✓</span> Refactored <span class="token-func">4</span> files, added <span class="token-func">12</span> tests' },
      { text: '  ✓ All tests passing', html: '  <span class="token-value">✓ All tests passing</span>' },
      { text: '', html: '' },
      { text: '$ xbot run  # 13 channels · always-on', html: '<span class="token-flag">$</span> <span class="token-keyword">xbot</span> run  <span class="token-comment"># 13 channels · always-on</span>' },
    ];
    var lineIdx = 0, charIdx = 0;
    var CHAR_DELAY = 14, LINE_PAUSE = 60;
    var finishedHtml = [];

    function escapeHtml(str) {
      return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    }

    function render() {
      var parts = finishedHtml.slice();
      if (lineIdx < lines.length) {
        var partial = escapeHtml(lines[lineIdx].text.substring(0, charIdx));
        parts.push(partial);
      }
      codeEl.innerHTML = parts.join('\n') + '<span class="typing-cursor"></span>';
    }

    function typeNext() {
      if (lineIdx >= lines.length) {
        render();
        return;
      }
      var line = lines[lineIdx];
      if (charIdx <= line.text.length) {
        render();
        charIdx++;
        setTimeout(typeNext, CHAR_DELAY);
      } else {
        finishedHtml.push(line.html);
        lineIdx++;
        charIdx = 0;
        render();
        setTimeout(typeNext, LINE_PAUSE);
      }
    }
    setTimeout(typeNext, 800);
  }

  /* Active nav */
  var currentPage = window.location.pathname.split('/').pop() || 'index.html';
  document.querySelectorAll('.nav-links a').forEach(function(link) {
    var href = link.getAttribute('href');
    if (href === currentPage || (currentPage === '' && href === 'index.html')) {
      link.classList.add('active');
    }
  });

  /* Mobile nav */
  var mobileToggle = document.querySelector('.mobile-toggle');
  var navLinks = document.querySelector('.nav-links');
  if (mobileToggle && navLinks) {
    mobileToggle.addEventListener('click', function() {
      navLinks.classList.toggle('mobile-open');
    });
    navLinks.querySelectorAll('a').forEach(function(a) {
      a.addEventListener('click', function() { navLinks.classList.remove('mobile-open'); });
    });
  }

  /* Cross-highlighting for architecture diagram */
  var highlightEls = document.querySelectorAll('[data-highlight-group]');
  if (highlightEls.length > 0) {
    highlightEls.forEach(function(el) {
      el.addEventListener('mouseenter', function() {
        var group = el.getAttribute('data-highlight-group');
        highlightEls.forEach(function(other) {
          if (other.getAttribute('data-highlight-group') === group) {
            other.classList.add('glow');
          } else {
            other.classList.add('dim');
          }
        });
      });
      el.addEventListener('mouseleave', function() {
        highlightEls.forEach(function(other) {
          other.classList.remove('glow');
          other.classList.remove('dim');
        });
      });
    });
  }
});
})();
