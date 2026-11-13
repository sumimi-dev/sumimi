/*!
 * Sumimi dashboard — a single, dependency-free file.
 *
 * What this is:
 *   The whole dashboard UI. It is rendered with plain DOM APIs, the timeseries chart
 *   is drawn by hand as SVG, and there is no chart library, no framework, no bundler
 *   and no build step. The file you read is the file the browser runs.
 *
 * How it is served:
 *   The Rust backend (`sumimi-service`) serves a tiny HTML shell that loads
 *   `<base>/sumimi-app.js` and `<base>/sumimi.css`. By default `<base>` is `/app`
 *   and the files are served from the `js/` directory of this repo. Setting
 *   `SUMIMI_APP_URL` points `<base>` at a CDN instead, so this file can be hosted
 *   anywhere and still drive a backend on another origin.
 *
 * Because of that, the API base URL is configurable and defaults to the origin the
 * app was served from. Override it with, in order of precedence:
 *   1. `window.SUMIMI_API_URL`
 *   2. `?api=<url>` in the page URL
 *   3. the `<meta name="sumimi-api-url">` tag in the page head
 *   4. the origin the app was served from
 *
 * Privacy:
 *   The only network requests this file makes are to the Sumimi API. There is no
 *   analytics, no tracker, no third-party asset, no web font and no CDN call.
 *
 * Note on storage:
 *   The admin token below is *the dashboard's own* credential — it lets the logged-in
 *   admin read the API. It is unrelated to the tracker (`sumimi.js`), which sets no
 *   storage at all: no cookies, no localStorage, no fingerprinting.
 */
(function () {
  'use strict';

  // --- configuration -------------------------------------------------------

  var TOKEN_KEY = 'sumimi.token';

  /** Time ranges offered by the picker, in milliseconds. */
  var RANGES = [
    { id: '24h', label: 'Last 24 hours', ms: 24 * 60 * 60 * 1000 },
    { id: '7d', label: 'Last 7 days', ms: 7 * 24 * 60 * 60 * 1000 },
    { id: '30d', label: 'Last 30 days', ms: 30 * 24 * 60 * 60 * 1000 },
    { id: '90d', label: 'Last 90 days', ms: 90 * 24 * 60 * 60 * 1000 }
  ];

  /** How many rows a breakdown table shows. */
  var BREAKDOWN_LIMIT = 15;

  /** How many rows the session / event logs show. */
  var LOG_LIMIT = 25;

  /**
   * The API base URL: no trailing slash.
   * Defaults to the origin the app was served from so a CDN-hosted app still talks
   * to the backend that served the page.
   */
  var API_URL = resolveApiUrl();

  /** Application state. */
  // The kinds of **marked** traffic: the traffic that is not part of the
  // numbers. The dashboard can ask for the kinds it wants to see.
  //
  // They come in groups: picking a group picks all of its kinds.
  //
  // The list is the server's (`MARKER_GROUPS` in `service/src/storage.rs`) — a
  // test checks that the two agree.
  var MARKER_GROUPS = [
    { kind: 'dev', label: 'dev' },
    { kind: 'test', label: 'test' },
    {
      kind: 'bot-ai',
      label: 'AI',
      kinds: [
        { kind: 'bot-ai-openai', label: 'OpenAI' },
        { kind: 'bot-ai-anthropic', label: 'Anthropic' },
        { kind: 'bot-ai-perplexity', label: 'Perplexity' },
        { kind: 'bot-ai-apple', label: 'Apple' }
      ]
    },
    {
      kind: 'bot-se',
      label: 'search',
      kinds: [
        { kind: 'bot-se-google', label: 'Google' },
        { kind: 'bot-se-bing', label: 'Bing' },
        { kind: 'bot-se-yandex', label: 'Yandex' },
        { kind: 'bot-se-baidu', label: 'Baidu' },
        { kind: 'bot-se-other', label: 'other' }
      ]
    },
    {
      kind: 'bot-social',
      label: 'social',
      kinds: [
        { kind: 'bot-social-meta', label: 'Meta' },
        { kind: 'bot-social-x', label: 'X' },
        { kind: 'bot-social-linkedin', label: 'LinkedIn' },
        { kind: 'bot-social-messaging', label: 'messaging' }
      ]
    },
    {
      kind: 'bot-other',
      label: 'other',
      kinds: [{ kind: 'bot-other-unknown', label: 'unknown' }]
    }
  ];

  /** The groups of marked traffic and their boxes. */
  var markerGroups = [];

  var state = {
    token: null,
    user: null,
    websites: [],
    websiteId: null,
    range: '24h',
    // The kinds of marked traffic to count anyway.
    markers: [],
    loading: false
  };

  /** Root element; the backend's HTML shell provides `<div id="app">`. */
  var root = document.getElementById('app') || document.body;

  // --- small helpers -------------------------------------------------------

  /**
   * Resolve the API base URL from the environment.
   * @returns {string}
   */
  function resolveApiUrl() {
    var candidate = '';

    if (typeof window.SUMIMI_API_URL === 'string' && window.SUMIMI_API_URL) {
      candidate = window.SUMIMI_API_URL;
    }

    if (!candidate) {
      try {
        var params = new URLSearchParams(window.location.search);
        candidate = params.get('api') || '';
      } catch (e) {
        candidate = '';
      }
    }

    if (!candidate) {
      var meta = document.querySelector('meta[name="sumimi-api-url"]');
      candidate = (meta && meta.getAttribute('content')) || '';
    }

    if (!candidate) {
      // Same-origin: the backend serves both the page and the API.
      candidate = window.location.origin;
    }

    return candidate.replace(/\/+$/, '');
  }

  /**
   * Create an element.
   * @param {string} tag
   * @param {object} [attrs]  properties set on the element (class, text, ...)
   * @param {Array} [children]
   * @returns {HTMLElement}
   */
  function el(tag, attrs, children) {
    var node = document.createElement(tag);
    if (attrs) {
      Object.keys(attrs).forEach(function (key) {
        var value = attrs[key];
        if (value === null || value === undefined || value === false) return;
        if (key === 'text') {
          node.textContent = String(value);
        } else if (key === 'html') {
          node.innerHTML = String(value);
        } else if (key === 'class') {
          node.className = String(value);
        } else if (key.indexOf('on') === 0 && typeof value === 'function') {
          node.addEventListener(key.slice(2), value);
        } else {
          node.setAttribute(key, value === true ? '' : String(value));
        }
      });
    }
    (children || []).forEach(function (child) {
      if (child === null || child === undefined) return;
      node.appendChild(typeof child === 'string' ? document.createTextNode(child) : child);
    });
    return node;
  }

  /** The label of a kind of marked traffic. */
  function labelOf(kind) {
    for (var i = 0; i < MARKER_GROUPS.length; i++) {
      var group = MARKER_GROUPS[i];
      if (group.kind === kind) return group.label;
      var kinds = group.kinds || [];
      for (var j = 0; j < kinds.length; j++) {
        if (kinds[j].kind === kind) return kinds[j].label;
      }
    }
    return kind;
  }

  /** Bring the boxes in line with what is picked. */
  function syncMarkerBoxes() {
    markerGroups.forEach(function (group) {
      group.children.forEach(function (child) {
        child.box.checked = state.markers.indexOf(child.leaf) !== -1;
      });
      var on = group.leaves.filter(function (leaf) {
        return state.markers.indexOf(leaf) !== -1;
      }).length;
      group.box.checked = on === group.leaves.length;
      group.box.indeterminate = on > 0 && on < group.leaves.length;
    });
  }

  /** Remove every child of a node. */
  function clear(node) {
    while (node && node.firstChild) node.removeChild(node.firstChild);
  }

  /**
   * Format a number with thousands separators.
   * @param {number} value
   * @returns {string}
   */
  function num(value) {
    if (value === null || value === undefined || isNaN(value)) return '0';
    return Number(value).toLocaleString('en-US');
  }

  /**
   * Format a duration in seconds as `1h 2m 3s`.
   * @param {number} seconds
   * @returns {string}
   */
  function duration(seconds) {
    var total = Math.max(0, Math.round(Number(seconds) || 0));
    var h = Math.floor(total / 3600);
    var m = Math.floor((total % 3600) / 60);
    var s = total % 60;
    if (h > 0) return h + 'h ' + m + 'm';
    if (m > 0) return m + 'm ' + s + 's';
    return s + 's';
  }

  /**
   * Format a timestamp for display. The backend sends naive UTC timestamps like
   * `2026-11-03T18:00:00` (no zone), so they are read as UTC and shown in the
   * viewer's local zone.
   * @param {string} value
   * @returns {string}
   */
  function time(value) {
    if (!value) return '—';
    var text = String(value);
    if (!/[zZ]|[+-]\d{2}:?\d{2}$/.test(text)) text += 'Z';
    var date = new Date(text);
    if (isNaN(date.getTime())) return String(value);
    return date.toLocaleString('en-US', {
      year: 'numeric',
      month: 'short',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit'
    });
  }

  /**
   * Shorten a URL for a table cell.
   * @param {string} value
   * @returns {string}
   */
  function shortUrl(value) {
    if (!value) return '—';
    var text = String(value);
    return text.length > 60 ? text.slice(0, 57) + '…' : text;
  }

  /** Escape-free text node helper for table cells. */
  function cell(text, className) {
    return el('td', { text: text === null || text === undefined || text === '' ? '—' : String(text), class: className || '' });
  }

  // --- API -----------------------------------------------------------------

  /**
   * An error with a status code and a message meant for humans.
   * @param {string} message
   * @param {number} [status]
   */
  function ApiError(message, status) {
    this.name = 'ApiError';
    this.message = message;
    this.status = status || 0;
  }
  ApiError.prototype = Object.create(Error.prototype);
  ApiError.prototype.constructor = ApiError;

  /**
   * Perform an API request.
   * @param {string} path      path relative to the API base
   * @param {object} [options] { method, body, auth }
   * @returns {Promise<any>}
   */
  function request(path, options) {
    var opts = options || {};
    var headers = { Accept: 'application/json' };

    if (opts.body !== undefined) headers['Content-Type'] = 'application/json';

    if (opts.auth !== false && state.token) {
      headers.Authorization = 'Bearer ' + state.token;
    }

    return fetch(API_URL + path, {
      method: opts.method || 'GET',
      headers: headers,
      body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
      credentials: 'omit',
      mode: 'cors'
    }).then(function (response) {
      return response.text().then(function (text) {
        var payload = null;
        if (text) {
          try {
            payload = JSON.parse(text);
          } catch (e) {
            payload = null;
          }
        }

        if (response.status === 401) {
          throw new ApiError('Your session expired. Please sign in again.', 401);
        }

        if (!response.ok) {
          var message = (payload && payload.error) || ('Request failed (' + response.status + ')');
          throw new ApiError(String(message), response.status);
        }

        return payload;
      });
    }).catch(function (error) {
      if (error instanceof ApiError) throw error;
      // Network failure, CORS, DNS, offline...
      throw new ApiError('Cannot reach the Sumimi API at ' + API_URL + '.', 0);
    });
  }

  /**
   * Build a stats query string for the current range.
   * @returns {string}
   */
  function rangeQuery() {
    var range = currentRange();
    var end = Date.now();
    var start = end - range.ms;
    var query = '?startAt=' + start + '&endAt=' + end;
    if (state.markers.length) {
      query += '&markers=' + encodeURIComponent(state.markers.join(','));
    }
    return query;
  }

  /** The currently selected range object. */
  function currentRange() {
    for (var i = 0; i < RANGES.length; i++) {
      if (RANGES[i].id === state.range) return RANGES[i];
    }
    return RANGES[0];
  }

  // --- storage -------------------------------------------------------------

  /**
   * Read the dashboard's own admin token from localStorage.
   * This is the dashboard credential, unrelated to the tracker, which stores nothing.
   */
  function readToken() {
    try {
      return window.localStorage.getItem(TOKEN_KEY) || null;
    } catch (e) {
      return null;
    }
  }

  /** Write the admin token to localStorage. */
  function writeToken(token) {
    try {
      window.localStorage.setItem(TOKEN_KEY, token);
    } catch (e) {
      /* private mode: the session simply will not survive a reload */
    }
  }

  /** Forget the admin token. */
  function clearToken() {
    try {
      window.localStorage.removeItem(TOKEN_KEY);
    } catch (e) {
      /* ignore */
    }
  }

  // --- branding ------------------------------------------------------------

  /**
   * The Sumimi paw logo.
   *
   * The geometry is defined once, in `docs/assets/logo.svg`; the documentation site and
   * this dashboard must show the same paw. A test (service/tests/logo.rs) checks that
   * every copy in the repository still matches the canonical file.
   */
  var LOGO_SVG = '<svg class="logo" viewBox="0 0 32 32" role="img" aria-label="Sumimi"><g fill="currentColor"><ellipse cx="7.4" cy="13.2" rx="2.9" ry="3.9" transform="rotate(-20 7.4 13.2)"/><ellipse cx="12.8" cy="8.8" rx="2.8" ry="3.9" transform="rotate(-7 12.8 8.8)"/><ellipse cx="19.2" cy="8.8" rx="2.8" ry="3.9" transform="rotate(7 19.2 8.8)"/><ellipse cx="24.6" cy="13.2" rx="2.9" ry="3.9" transform="rotate(20 24.6 13.2)"/><path d="M9.3 21.2c0-2.8 3-5 6.7-5s6.7 2.2 6.7 5c0 2.5-2.1 4.2-4.4 4.2-.9 0-1.6-.3-2.3-.3s-1.4.3-2.3.3c-2.3 0-4.4-1.7-4.4-4.2z"/></g></svg>';

  /** @returns {SVGElement} */
  function pawLogo() {
    var holder = document.createElement('span');
    holder.innerHTML = LOGO_SVG;
    return holder.firstChild;
  }

  // --- views ---------------------------------------------------------------

  /** A short, human-readable error message. Never a stack trace. */
  function errorMessage(error) {
    if (error instanceof ApiError) return error.message;
    if (error && typeof error.message === 'string' && error.message) return error.message;
    return 'Something went wrong.';
  }

  /**
   * Render the login screen.
   * @param {string} [message]  an error to show
   */
  function renderLogin(message) {
    clear(root);

    var username = el('input', {
      type: 'text',
      id: 'sumimi-username',
      name: 'username',
      autocomplete: 'username',
      autocapitalize: 'off',
      spellcheck: 'false',
      required: true,
      class: 'field-input'
    });

    var password = el('input', {
      type: 'password',
      id: 'sumimi-password',
      name: 'password',
      autocomplete: 'current-password',
      required: true,
      class: 'field-input'
    });

    var error = el('p', { class: 'form-error', role: 'alert', id: 'sumimi-login-error' });
    if (message) error.textContent = message;

    var submit = el('button', { type: 'submit', class: 'btn btn-primary' }, ['Sign in']);

    var form = el(
      'form',
      {
        class: 'login-form',
        novalidate: false,
        onsubmit: function (event) {
          event.preventDefault();
          login(username.value.trim(), password.value, error, submit);
        }
      },
      [
        error,
        el('div', { class: 'field' }, [
          el('label', { for: 'sumimi-username', text: 'Username' }),
          username
        ]),
        el('div', { class: 'field' }, [
          el('label', { for: 'sumimi-password', text: 'Password' }),
          password
        ]),
        submit
      ]
    );

    var card = el('div', { class: 'login-card' }, [
      el('div', { class: 'brand brand-login' }, [pawLogo(), el('div', {}, [
        el('span', { class: 'brand-name', text: 'Sumimi' }),
        el('span', { class: 'brand-cn', text: '数咪咪' })
      ])]),
      el('p', { class: 'brand-slogan', text: 'Count kitties popping into my websites.' }),
      form
    ]);

    root.appendChild(el('div', { class: 'login-wrap' }, [card]));

    username.focus();
  }

  /**
   * Try to sign in.
   * @param {string} username
   * @param {string} password
   * @param {HTMLElement} errorBox
   * @param {HTMLElement} submit
   */
  function login(username, password, errorBox, submit) {
    if (!username || !password) {
      errorBox.textContent = 'Enter both a username and a password.';
      return;
    }

    errorBox.textContent = '';
    submit.disabled = true;
    submit.textContent = 'Signing in…';

    request('/api/login', {
      method: 'POST',
      auth: false,
      body: { username: username, password: password }
    })
      .then(function (result) {
        if (!result || !result.token) throw new ApiError('The server did not return a token.', 0);
        state.token = result.token;
        state.user = result.user || null;
        writeToken(state.token);
        start();
      })
      .catch(function (error) {
        submit.disabled = false;
        submit.textContent = 'Sign in';
        errorBox.textContent = errorMessage(error);
        password.focus();
      });
  }

  /**
   * Render the dashboard shell: header, controls and an empty body that is
   * filled in by `loadDashboard`.
   */
  function renderShell() {
    clear(root);

    var siteSelect = el('select', {
      id: 'sumimi-website',
      class: 'select',
      onchange: function () {
        state.websiteId = siteSelect.value || null;
        loadDashboard();
      }
    });

    var rangeGroup = el('div', { class: 'range-group', role: 'group', 'aria-label': 'Time range' },
      RANGES.map(function (range) {
        var button = el('button', {
          type: 'button',
          class: 'range-btn',
          'data-range': range.id,
          text: range.id,
          title: range.label,
          'aria-label': range.label,
          'aria-pressed': range.id === state.range ? 'true' : 'false',
          onclick: function () {
            if (state.range === range.id) return;
            state.range = range.id;
            syncRangeButtons();
            loadDashboard();
          }
        });
        return button;
      })
    );

    // The **marked** traffic — the bots, the developer's own clicks — is not
    // part of the numbers. The dropdown asks for the kinds it wants to see:
    // a group and the kinds in it.
    //
    // The groups are rebuilt every time the shell is rendered ⇒ the old ones
    // are dropped first.
    markerGroups = [];
    var markedSelect = el('details', { class: 'marked-select', id: 'sumimi-markers' }, [
      el('summary', { text: 'Marked' }),
      el('div', { class: 'marked-list' }, MARKER_GROUPS.map(function (group) {
        var kinds = group.kinds || [{ kind: group.kind, label: group.label }];
        var leaves = kinds.map(function (kind) { return kind.kind; });

        var groupBox = el('input', { type: 'checkbox' });
        var children = kinds.map(function (kind) {
          var box = el('input', { type: 'checkbox' });
          box.addEventListener('change', function () {
            if (box.checked) {
              if (state.markers.indexOf(kind.kind) === -1) state.markers.push(kind.kind);
            } else {
              state.markers = state.markers.filter(function (leaf) {
                return leaf !== kind.kind;
              });
            }
            syncMarkerBoxes();
            loadDashboard();
          });
          return { box: box, leaf: kind.kind };
        });

        groupBox.addEventListener('change', function () {
          // Picking the group picks all of its kinds; unpicking unpicks them.
          state.markers = state.markers.filter(function (leaf) {
            return leaves.indexOf(leaf) === -1;
          });
          if (groupBox.checked) state.markers = state.markers.concat(leaves);
          syncMarkerBoxes();
          loadDashboard();
        });

        markerGroups.push({ box: groupBox, leaves: leaves, children: children });

        return el('div', { class: 'marked-group' }, [
          el('label', { class: 'marked-item' }, [groupBox, group.label])
        ].concat(children.map(function (child) {
          return el('label', { class: 'marked-item marked-sub' }, [child.box, labelOf(child.leaf)]);
        })));
      }))
    ]);

    var logout = el('button', {
      type: 'button',
      class: 'btn btn-ghost',
      onclick: function () {
        clearToken();
        state.token = null;
        state.user = null;
        state.websites = [];
        state.websiteId = null;
        renderLogin('');
      }
    }, ['Log out']);

    var userName = el('span', {
      class: 'user-name',
      text: state.user && state.user.username ? state.user.username : ''
    });

    var body = el('div', { id: 'dashboard-body', class: 'dashboard-body' });

    var header = el('header', { class: 'topbar' }, [
      el('div', { class: 'brand' }, [pawLogo(), el('div', {}, [
        el('span', { class: 'brand-name', text: 'Sumimi' }),
        el('span', { class: 'brand-cn', text: '数咪咪' })
      ])]),
      el('div', { class: 'topbar-controls' }, [
        el('div', { class: 'field field-inline' }, [
          el('label', { for: 'sumimi-website', text: 'Website' }),
          siteSelect
        ]),
        el('div', { class: 'field field-inline' }, [
          el('span', { class: 'label-like', id: 'sumimi-range-label', text: 'Range' }),
          rangeGroup
        ]),
        el('div', { class: 'field field-inline' }, [
          el('span', { class: 'label-like', text: 'Marked' }),
          markedSelect
        ])
      ]),
      el('div', { class: 'topbar-right' }, [userName, logout])
    ]);

    root.appendChild(el('div', { class: 'app-shell' }, [header, body]));

    // Keep references for later updates.
    root._sumimi = { siteSelect: siteSelect, rangeGroup: rangeGroup, body: body };
  }

  /** Update the pressed state of the range buttons. */
  function syncRangeButtons() {
    var group = root._sumimi && root._sumimi.rangeGroup;
    if (!group) return;
    Array.prototype.forEach.call(group.querySelectorAll('.range-btn'), function (button) {
      var active = button.getAttribute('data-range') === state.range;
      button.setAttribute('aria-pressed', active ? 'true' : 'false');
      button.classList.toggle('is-active', active);
    });
  }

  /**
   * Load the website list and populate the picker.
   * @returns {Promise<void>}
   */
  function loadWebsites() {
    return request('/api/websites').then(function (websites) {
      state.websites = Array.isArray(websites) ? websites : [];
      var select = root._sumimi && root._sumimi.siteSelect;
      if (!select) return;

      clear(select);

      if (!state.websites.length) {
        select.appendChild(el('option', { value: '', text: 'No websites yet' }));
        select.disabled = true;
        return;
      }

      select.disabled = false;
      state.websites.forEach(function (website) {
        select.appendChild(el('option', {
          value: website.id,
          text: website.name + (website.domain ? ' — ' + website.domain : '')
        }));
      });

      var current = state.websites.filter(function (w) {
        return w.id === state.websiteId;
      })[0];

      if (!current) {
        current = state.websites[0];
        state.websiteId = current.id;
      }

      select.value = state.websiteId;
    });
  }

  /**
   * Load everything the dashboard shows for the current website and range.
   */
  function loadDashboard() {
    var body = root._sumimi && root._sumimi.body;
    if (!body) return;

    if (!state.websiteId) {
      clear(body);
      body.appendChild(el('div', { class: 'empty' }, [
        el('p', { class: 'empty-title', text: 'No website selected' }),
        el('p', { class: 'empty-hint', text: 'Create a website in Sumimi, then pick it here.' })
      ]));
      return;
    }

    state.loading = true;
    var id = encodeURIComponent(state.websiteId);
    var query = rangeQuery();

    var stats = request('/api/websites/' + id + '/stats' + query);
    var series = request('/api/websites/' + id + '/stats/timeseries' + query);
    var pages = request('/api/websites/' + id + '/stats/pages' + query + '&limit=' + BREAKDOWN_LIMIT);
    var referrers = request('/api/websites/' + id + '/stats/referrers' + query + '&limit=' + BREAKDOWN_LIMIT);
    var browsers = request('/api/websites/' + id + '/stats/browsers' + query + '&limit=' + BREAKDOWN_LIMIT);
    var os = request('/api/websites/' + id + '/stats/os' + query + '&limit=' + BREAKDOWN_LIMIT);
    var devices = request('/api/websites/' + id + '/stats/devices' + query + '&limit=' + BREAKDOWN_LIMIT);
    var countries = request('/api/websites/' + id + '/stats/countries' + query + '&limit=' + BREAKDOWN_LIMIT);
    var languages = request('/api/websites/' + id + '/stats/languages' + query + '&limit=' + BREAKDOWN_LIMIT);
    var screens = request('/api/websites/' + id + '/stats/screens' + query + '&limit=' + BREAKDOWN_LIMIT);
    var events = request('/api/websites/' + id + '/stats/events' + query + '&limit=' + BREAKDOWN_LIMIT);
    var sessions = request('/api/websites/' + id + '/sessions' + query + '&limit=' + LOG_LIMIT);
    var eventLog = request('/api/websites/' + id + '/events' + query + '&limit=' + LOG_LIMIT);

    Promise.all([stats, series, pages, referrers, browsers, os, devices, countries, languages, screens, events, sessions, eventLog])
      .then(function (results) {
        state.loading = false;
        renderDashboard({
          stats: results[0],
          series: results[1],
          pages: results[2],
          referrers: results[3],
          browsers: results[4],
          os: results[5],
          devices: results[6],
          countries: results[7],
          languages: results[8],
          screens: results[9],
          events: results[10],
          sessions: results[11],
          eventLog: results[12]
        });
      })
      .catch(function (error) {
        state.loading = false;
        if (error instanceof ApiError && error.status === 401) {
          handleUnauthorized();
          return;
        }
        clear(body);
        body.appendChild(el('div', { class: 'empty' }, [
          el('p', { class: 'empty-title', text: 'Could not load the dashboard' }),
          el('p', { class: 'empty-hint', text: errorMessage(error) })
        ]));
      });
  }

  /** Clear the session and go back to the login screen. */
  function handleUnauthorized() {
    clearToken();
    state.token = null;
    state.user = null;
    renderLogin('Your session expired. Please sign in again.');
  }

  /**
   * Render the dashboard body.
   * @param {object} data
   */
  function renderDashboard(data) {
    var body = root._sumimi && root._sumimi.body;
    if (!body) return;
    clear(body);

    var range = currentRange();

    body.appendChild(el('p', { class: 'range-note', text: range.label }));

    body.appendChild(el('section', { class: 'tiles', 'aria-label': 'Summary metrics' }, [
      metricTile('Pageviews', data.stats && data.stats.pageviews, 'pageviews'),
      metricTile('Visitors', data.stats && data.stats.visitors, 'visitors'),
      metricTile('Views', data.stats && data.stats.views, 'views'),
      metricTile('Bounces', data.stats && data.stats.bounces, 'bounces'),
      metricTile('Avg. visit duration', data.stats && data.stats.visitDuration, 'duration')
    ]));

    var chart = el('section', { class: 'card card-wide' }, [
      el('h2', { class: 'card-title', text: 'Pageviews over time' }),
      el('div', { class: 'chart-wrap', id: 'sumimi-chart' })
    ]);
    body.appendChild(chart);
    drawChart(document.getElementById('sumimi-chart'), data.series || []);

    var grid = el('div', { class: 'grid' });

    grid.appendChild(breakdownCard('Top pages', data.pages, 'url'));
    grid.appendChild(breakdownCard('Referrers', data.referrers, 'name'));
    grid.appendChild(breakdownCard('Browsers', data.browsers, 'name'));
    grid.appendChild(breakdownCard('Operating systems', data.os, 'name'));
    grid.appendChild(breakdownCard('Devices', data.devices, 'name'));
    grid.appendChild(breakdownCard('Countries', data.countries, 'name'));
    grid.appendChild(breakdownCard('Languages', data.languages, 'name'));
    grid.appendChild(breakdownCard('Screens', data.screens, 'name'));
    grid.appendChild(breakdownCard('Custom events', data.events, 'name'));

    body.appendChild(grid);

    body.appendChild(sessionsCard(data.sessions || []));
    body.appendChild(eventsCard(data.eventLog || []));
  }

  /**
   * One metric tile.
   * @param {string} label
   * @param {object} metric  { value, change }
   * @param {string} kind    'pageviews' | 'visitors' | 'views' | 'bounces' | 'duration'
   * @returns {HTMLElement}
   */
  function metricTile(label, metric, kind) {
    var value = metric && typeof metric.value === 'number' ? metric.value : 0;
    var change = metric && typeof metric.change === 'number' ? metric.change : 0;
    var shown = kind === 'duration' ? duration(value) : num(value);

    var direction = change > 0 ? 'up' : change < 0 ? 'down' : 'flat';
    // For bounces a rise is not good news, so the colour is inverted.
    var positive = kind === 'bounces' ? change <= 0 : change >= 0;

    var delta = el('span', {
      class: 'tile-delta tile-delta-' + direction + (positive ? ' is-positive' : ' is-negative'),
      text: (change > 0 ? '+' : '') + change.toFixed(1) + '%'
    });

    return el('article', { class: 'tile' }, [
      el('h3', { class: 'tile-label', text: label }),
      el('p', { class: 'tile-value', text: shown }),
      el('p', { class: 'tile-change' }, [delta, el('span', { class: 'tile-change-note', text: 'vs. previous period' })])
    ]);
  }

  /**
   * A breakdown card: a small table of name / value rows.
   * @param {string} title
   * @param {Array} rows
   * @param {string} key  'name' or 'url'
   * @returns {HTMLElement}
   */
  function breakdownCard(title, rows, key) {
    var list = Array.isArray(rows) ? rows : [];

    var card = el('section', { class: 'card' }, [
      el('h2', { class: 'card-title', text: title })
    ]);

    if (!list.length) {
      card.appendChild(el('p', { class: 'empty-hint', text: 'No data for this period.' }));
      return card;
    }

    var table = el('table', { class: 'table' });
    var tbody = el('tbody');

    list.forEach(function (row) {
      var label = key === 'url' ? (row.url || row.name || '') : row.name;
      var tr = el('tr', {}, [
        el('th', { scope: 'row', class: 'table-name', title: String(label || '') }, [
          el('span', { text: shortUrl(label) })
        ]),
        cell(num(row.value), 'table-value')
      ]);
      tbody.appendChild(tr);
    });

    table.appendChild(tbody);
    card.appendChild(table);
    return card;
  }

  /**
   * The session log.
   * @param {Array} sessions
   * @returns {HTMLElement}
   */
  function sessionsCard(sessions) {
    var card = el('section', { class: 'card card-wide' }, [
      el('h2', { class: 'card-title', text: 'Sessions' })
    ]);

    if (!sessions.length) {
      card.appendChild(el('p', { class: 'empty-hint', text: 'No sessions in this period.' }));
      return card;
    }

    var table = el('table', { class: 'table table-wide' });
    table.appendChild(el('thead', {}, [
      el('tr', {}, [
        el('th', { scope: 'col', text: 'Time' }),
        el('th', { scope: 'col', text: 'URL' }),
        el('th', { scope: 'col', text: 'Referrer' }),
        el('th', { scope: 'col', text: 'Browser' }),
        el('th', { scope: 'col', text: 'OS' }),
        el('th', { scope: 'col', text: 'Device' }),
        el('th', { scope: 'col', text: 'Country' }),
        el('th', { scope: 'col', class: 'num', text: 'Views' }),
        el('th', { scope: 'col', class: 'num', text: 'Duration' })
      ])
    ]));

    var tbody = el('tbody');
    sessions.forEach(function (session) {
      tbody.appendChild(el('tr', {}, [
        cell(time(session.createdAt), 'nowrap'),
        el('td', { title: String(session.url || '') }, [
          el('span', { class: 'truncate', text: shortUrl(session.url) })
        ]),
        el('td', { title: String(session.referrer || '') }, [
          el('span', { class: 'truncate', text: shortUrl(session.referrer) })
        ]),
        cell(session.browser),
        cell(session.os),
        cell(session.device),
        cell(session.country),
        cell(num(session.views), 'num'),
        cell(duration(session.duration), 'num nowrap')
      ]));
    });

    table.appendChild(tbody);
    card.appendChild(table);
    return card;
  }

  /**
   * The custom event log.
   * @param {Array} events
   * @returns {HTMLElement}
   */
  function eventsCard(events) {
    var card = el('section', { class: 'card card-wide' }, [
      el('h2', { class: 'card-title', text: 'Event log' })
    ]);

    if (!events.length) {
      card.appendChild(el('p', { class: 'empty-hint', text: 'No custom events in this period.' }));
      return card;
    }

    var table = el('table', { class: 'table table-wide' });
    table.appendChild(el('thead', {}, [
      el('tr', {}, [
        el('th', { scope: 'col', text: 'Time' }),
        el('th', { scope: 'col', text: 'Name' }),
        el('th', { scope: 'col', text: 'URL' }),
        el('th', { scope: 'col', text: 'Data' })
      ])
    ]));

    var tbody = el('tbody');
    events.forEach(function (event) {
      tbody.appendChild(el('tr', {}, [
        cell(time(event.createdAt), 'nowrap'),
        cell(event.name),
        el('td', { title: String(event.url || '') }, [
          el('span', { class: 'truncate', text: shortUrl(event.url) })
        ]),
        el('td', { title: String(event.data || '') }, [
          el('span', { class: 'truncate', text: shortUrl(event.data) })
        ])
      ]));
    });

    table.appendChild(tbody);
    card.appendChild(table);
    return card;
  }

  // --- chart ----------------------------------------------------------------

  var SVG_NS = 'http://www.w3.org/2000/svg';

  /**
   * Draw the pageviews timeseries by hand as SVG.
   * No chart library, no canvas: plain paths, ticks and labels.
   * @param {HTMLElement} host
   * @param {Array} series  [{ name, value, date }]
   */
  function drawChart(host, series) {
    if (!host) return;
    clear(host);
    // Remember the data so the chart can be redrawn on resize.
    host._sumimiSeries = series;

    if (!series.length) {
      host.appendChild(el('p', { class: 'empty-hint', text: 'No pageviews in this period.' }));
      return;
    }

    var width = Math.max(host.clientWidth || 0, 320);
    var height = 260;
    var pad = { top: 16, right: 16, bottom: 30, left: 52 };

    var svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('viewBox', '0 0 ' + width + ' ' + height);
    svg.setAttribute('class', 'chart');
    svg.setAttribute('role', 'img');
    svg.setAttribute('aria-label', 'Pageviews over time');
    svg.setAttribute('preserveAspectRatio', 'none');

    var values = series.map(function (point) { return Number(point.value) || 0; });
    var scale = niceScale(values);

    var max = scale.max;
    var min = scale.min;

    var plotW = width - pad.left - pad.right;
    var plotH = height - pad.top - pad.bottom;

    /** Map a data index to an x pixel. */
    function xAt(index) {
      if (series.length === 1) return pad.left + plotW / 2;
      return pad.left + (index / (series.length - 1)) * plotW;
    }

    /** Map a value to a y pixel. */
    function yAt(value) {
      return pad.top + plotH - ((value - min) / (max - min)) * plotH;
    }

    // --- horizontal grid lines + y-axis labels ---
    // Ticks sit on multiples of the nice step, so they read as round numbers.
    var step = scale.step;
    var firstTick = Math.ceil(min / step) * step;
    var lastTick = null;
    for (var value = firstTick; value <= max + step / 1000; value += step) {
      var y = yAt(value);

      var line = document.createElementNS(SVG_NS, 'line');
      line.setAttribute('x1', String(pad.left));
      line.setAttribute('y1', String(y));
      line.setAttribute('x2', String(width - pad.right));
      line.setAttribute('y2', String(y));
      line.setAttribute('class', 'chart-grid');
      svg.appendChild(line);

      // Skip a label when it would repeat the previous one (small ranges).
      var text = num(Math.round(value));
      if (text === lastTick) continue;
      lastTick = text;

      var label = document.createElementNS(SVG_NS, 'text');
      label.setAttribute('x', String(pad.left - 8));
      label.setAttribute('y', String(y + 4));
      label.setAttribute('class', 'chart-axis');
      label.setAttribute('text-anchor', 'end');
      label.textContent = text;
      svg.appendChild(label);
    }

    // --- the area under the line ---
    var points = series.map(function (point, index) {
      return xAt(index) + ',' + yAt(Number(point.value) || 0);
    });

    var area = document.createElementNS(SVG_NS, 'path');
    area.setAttribute(
      'd',
      'M' + pad.left + ',' + (pad.top + plotH) +
        ' L' + points.join(' L') +
        ' L' + xAt(series.length - 1) + ',' + (pad.top + plotH) + ' Z'
    );
    area.setAttribute('class', 'chart-area');
    svg.appendChild(area);

    // --- the line itself ---
    var line = document.createElementNS(SVG_NS, 'path');
    line.setAttribute('d', 'M' + points.join(' L'));
    line.setAttribute('class', 'chart-line');
    svg.appendChild(line);

    // --- points ---
    series.forEach(function (point, index) {
      var dot = document.createElementNS(SVG_NS, 'circle');
      dot.setAttribute('cx', String(xAt(index)));
      dot.setAttribute('cy', String(yAt(Number(point.value) || 0)));
      dot.setAttribute('r', '2.5');
      dot.setAttribute('class', 'chart-dot');
      var title = document.createElementNS(SVG_NS, 'title');
      title.textContent = (point.date || point.name || '') + ' — ' + num(point.value) + ' pageviews';
      dot.appendChild(title);
      svg.appendChild(dot);
    });

    // --- x-axis labels (first, middle, last) ---
    // The first and last labels are anchored inwards so they never run off
    // the plot or collide with the y-axis labels.
    var labelIndexes = [0];
    if (series.length > 2) labelIndexes.push(Math.floor((series.length - 1) / 2));
    labelIndexes.push(series.length - 1);

    // With a single point, first and last are the same bucket: label it once.
    if (labelIndexes.length === 2 && labelIndexes[0] === labelIndexes[1]) {
      labelIndexes = [labelIndexes[0]];
    }

    labelIndexes.forEach(function (index, position) {
      var point = series[index];
      var text = document.createElementNS(SVG_NS, 'text');
      text.setAttribute('x', String(xAt(index)));
      text.setAttribute('y', String(height - 10));
      text.setAttribute('class', 'chart-axis');

      if (labelIndexes.length === 1) {
        text.setAttribute('text-anchor', 'middle');
      } else if (position === 0) {
        text.setAttribute('text-anchor', 'start');
      } else if (position === labelIndexes.length - 1) {
        text.setAttribute('text-anchor', 'end');
      } else {
        text.setAttribute('text-anchor', 'middle');
      }

      text.textContent = axisLabel(point.date || point.name, series.length === 1);
      svg.appendChild(text);
    });

    host.appendChild(svg);
  }

  /**
   * Round a value range out to friendly tick bounds (0, 5, 10, 25, 50, 100, ...).
   * @param {Array<number>} values
   * @returns {{min: number, max: number, step: number}}
   */
  function niceScale(values) {
    var max = Math.max.apply(null, values);
    var min = Math.min.apply(null, values);

    if (!isFinite(max) || !isFinite(min)) return { min: 0, max: 1, step: 0.25 };
    if (max === min) {
      // A flat series: give it headroom above the value.
      if (max <= 0) return { min: 0, max: 1, step: 0.25 };
      min = 0;
      max = max * 1.2;
    }

    if (min > 0) min = 0; // counts start at zero

    var span = max - min;
    var step = niceStep(span / 4);
    var niceMin = Math.floor(min / step) * step;
    var niceMax = Math.ceil(max / step) * step;

    if (niceMax === niceMin) niceMax = niceMin + step;

    return { min: niceMin, max: niceMax, step: step };
  }

  /**
   * Round a raw step up to the nearest 1, 2, 5 or 10 times a power of ten.
   * @param {number} raw
   * @returns {number}
   */
  function niceStep(raw) {
    if (!isFinite(raw) || raw <= 0) return 1;
    var magnitude = Math.pow(10, Math.floor(Math.log(raw) / Math.LN10));
    var scaled = raw / magnitude;
    var step;
    if (scaled <= 1) step = 1;
    else if (scaled <= 2) step = 2;
    else if (scaled <= 5) step = 5;
    else step = 10;
    return step * magnitude;
  }

  /**
   * A short axis label for a bucket such as `2026-11-03T18:00:00Z`.
   * @param {string} value
   * @param {boolean} single
   * @returns {string}
   */
  function axisLabel(value, single) {
    if (!value) return '';
    var text = String(value);
    var date = new Date(/[zZ]|[+-]\d{2}:?\d{2}$/.test(text) ? text : text + 'Z');
    if (isNaN(date.getTime())) return text.slice(0, 10);

    if (single) {
      return date.toLocaleString('en-US', { month: 'short', day: '2-digit', hour: '2-digit', minute: '2-digit' });
    }

    // Hourly buckets get a time, daily buckets get a date.
    if (/T\d{2}:/.test(text)) {
      return date.toLocaleString('en-US', { month: 'short', day: '2-digit', hour: '2-digit' });
    }
    return date.toLocaleDateString('en-US', { month: 'short', day: '2-digit' });
  }

  // --- boot -----------------------------------------------------------------

  /**
   * Give the tab the Sumimi paw as its favicon.
   * The icon is an inline SVG data URI: no file, no network request.
   */
  function installFavicon() {
    var svg =
      "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'>" +
      "<g fill='%23b4520d'>" +
      "<ellipse cx='8.4' cy='11.5' rx='3.1' ry='3.9'/>" +
      "<ellipse cx='14' cy='8.2' rx='3' ry='4.2'/>" +
      "<ellipse cx='20.4' cy='9' rx='3' ry='4'/>" +
      "<ellipse cx='25' cy='13.6' rx='2.8' ry='3.5'/>" +
      "<path d='M16 15.4c4.6 0 8.3 3.3 8.3 7.1 0 3.1-2.5 4.9-5.5 4.9-1 0-1.9-.3-2.8-.3s-1.8.3-2.8.3c-3 0-5.5-1.8-5.5-4.9 0-3.8 3.7-7.1 8.3-7.1z'/>" +
      "</g></svg>";

    var link = document.querySelector('link[rel~="icon"]');
    if (!link) {
      link = document.createElement('link');
      link.setAttribute('rel', 'icon');
      document.head.appendChild(link);
    }
    link.setAttribute('type', 'image/svg+xml');
    link.setAttribute('href', 'data:image/svg+xml,' + svg);
  }

  /**
   * Start the app: try the stored token, else show the login screen.
   */
  function start() {
    var token = readToken();
    if (token) {
      state.token = token;
      request('/api/auth/refresh', { method: 'POST' })
        .then(function (result) {
          state.token = result.token || state.token;
          state.user = result.user || null;
          writeToken(state.token);
          enterDashboard();
        })
        .catch(function (error) {
          if (error instanceof ApiError && error.status === 401) {
            handleUnauthorized();
            return;
          }
          clearToken();
          state.token = null;
          renderLogin(errorMessage(error));
        });
    } else {
      renderLogin('');
    }
  }

  /** Render the shell and load the data. */
  function enterDashboard() {
    renderShell();
    syncRangeButtons();
    loadWebsites()
      .then(loadDashboard)
      .catch(function (error) {
        if (error instanceof ApiError && error.status === 401) {
          handleUnauthorized();
          return;
        }
        var body = root._sumimi && root._sumimi.body;
        if (body) {
          clear(body);
          body.appendChild(el('div', { class: 'empty' }, [
            el('p', { class: 'empty-title', text: 'Could not load websites' }),
            el('p', { class: 'empty-hint', text: errorMessage(error) })
          ]));
        }
      });
  }

  // Redraw the chart when the window changes size.
  var resizeTimer = null;
  window.addEventListener('resize', function () {
    if (resizeTimer) window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(function () {
      var host = document.getElementById('sumimi-chart');
      if (host && host._sumimiSeries) drawChart(host, host._sumimiSeries);
    }, 150);
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () {
      installFavicon();
      start();
    });
  } else {
    installFavicon();
    start();
  }
})();
