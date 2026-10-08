import {
  SseDecoder, ApprovalLedger, normalizeApproval, selectedAfterRefresh,
  matchesSessionQuery, messageParts, eventMatchesFilter, relativeTime,
  formatNumber, formatDuration, parseContext, messageImageSources,
  parseThemeInput, validateThemeTokens, THEME_TOKENS, safeImageSource,
  mediaPath, rasterMediaType, parseImageEvent, validateSkillSource,
} from './core.mjs';

const $ = (id) => document.getElementById(id);
const state = {
  selectedSessionId: null,
  serverSessionId: null,
  sessions: [],
  status: null,
  messages: [],
  tab: 'conversation',
  sending: false,
  token: '',
  paused: new Map(),
  events: [],
  stream: null,
  transcriptVersion: 0,
  mediaObjectUrls: new Map(),
  modelsLoaded: false,
  skillsExpanded: false,
  refreshRunning: false,
  studio: null,
  studioLoaded: false,
  studioRequestVersion: 0,
  studioMutation: false,
  attachments: [],
  attachmentGeneration: 0,
  themeTokens: {},
};
const approvals = new ApprovalLedger();
let socket = null;
let socketGeneration = 0;
let rpcSequence = 0;
const rpcPending = new Map();
let toastTimer;
let refreshTimer;
let modelsPromise;

function icon(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('class', 'icon');
  svg.setAttribute('aria-hidden', 'true');
  const use = document.createElementNS(svg.namespaceURI, 'use');
  use.setAttribute('href', `/assets/icons.svg#${name}`);
  svg.append(use);
  return svg;
}

function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function toast(text) {
  clearTimeout(toastTimer);
  $('toast').textContent = text;
  $('toast').hidden = false;
  toastTimer = setTimeout(() => { $('toast').hidden = true; }, 3500);
}

function setError(element, text = '') {
  element.textContent = text;
  element.hidden = !text;
}

function errorText(error) {
  return error instanceof Error ? error.message : String(error);
}

function authHeaders(headers = {}) {
  const result = new Headers(headers);
  if (state.token) result.set('Authorization', `Bearer ${state.token}`);
  return result;
}

async function api(path, options = {}) {
  const response = await fetch(path, { ...options, headers: authHeaders(options.headers) });
  if (response.status === 401) throw new Error('Authentication required. Add the dashboard token in Connection.');
  if (!response.ok) throw new Error(`Request failed with HTTP ${response.status}.`);
  const result = await response.json();
  if (result?.ok === false) throw new Error(result.error || 'The server rejected this request.');
  return result;
}

function assertOk(result) {
  if (!result || result.ok === false) throw new Error(result?.error || 'The server rejected this request.');
  return result;
}

function updateConnection(phase) {
  const labels = { connecting: 'Connecting', live: 'Connected', offline: 'Offline', error: 'Connection failed' };
  $('connection-status').textContent = labels[phase] || phase;
  $('connection-dot').className = `status-dot ${phase === 'live' ? 'good' : phase === 'error' ? 'bad' : 'waiting'}`;
  $('gateway-label').textContent = phase === 'live' ? 'Live' : labels[phase] || phase;
  $('gateway-label').className = `state-label ${phase === 'live' ? 'good' : phase === 'error' ? 'bad' : ''}`;
  $('gateway-connect').querySelector('span').textContent = phase === 'live' ? 'Disconnect' : 'Connect';
  $('gateway-ping').disabled = phase !== 'live';
  renderControls();
}

function connected() { return socket?.readyState === WebSocket.OPEN; }

function rejectRpc(message) {
  for (const entry of rpcPending.values()) {
    clearTimeout(entry.timer);
    entry.reject(new Error(message));
  }
  rpcPending.clear();
}

function rpc(method, params = {}) {
  if (!connected()) return Promise.reject(new Error('Gateway is offline. Connect to continue.'));
  const id = `ui-${++rpcSequence}`;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      rpcPending.delete(id);
      reject(new Error('The gateway did not answer in time.'));
    }, 10_000);
    rpcPending.set(id, { resolve, reject, timer });
    try { socket.send(JSON.stringify({ jsonrpc: '2.0', id, method, params })); }
    catch (error) { clearTimeout(timer); rpcPending.delete(id); reject(error); }
  });
}

async function connectGateway() {
  if (socket) socket.close();
  rejectRpc('Connection was restarted.');
  const generation = ++socketGeneration;
  const url = new URL('/ws/gateway', location.href);
  url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  if (state.token) url.searchParams.set('token', state.token);
  updateConnection('connecting');
  const candidate = new WebSocket(url);
  socket = candidate;
  return new Promise((resolve, reject) => {
    let opened = false;
    const timeout = setTimeout(() => { candidate.close(); reject(new Error('Gateway connection timed out.')); }, 10_000);
    candidate.onopen = () => {
      if (generation !== socketGeneration) { candidate.close(); return; }
      clearTimeout(timeout);
      opened = true;
      updateConnection('live');
      resolve();
    };
    candidate.onerror = () => {
      if (generation !== socketGeneration) return;
      updateConnection('error');
    };
    candidate.onclose = () => {
      clearTimeout(timeout);
      if (generation !== socketGeneration) return;
      socket = null;
      rejectRpc('Gateway connection was closed.');
      updateConnection('offline');
      if (!opened) reject(new Error('Gateway connection failed. Check the dashboard token.'));
    };
    candidate.onmessage = ({ data }) => {
      if (generation !== socketGeneration) return;
      let message;
      try { message = JSON.parse(data); } catch { return; }
      if (message.method === 'event') { handleGatewayEvent(message.params); return; }
      const entry = rpcPending.get(String(message.id));
      if (!entry) return;
      clearTimeout(entry.timer);
      rpcPending.delete(String(message.id));
      if (message.error) entry.reject(new Error(message.error.message || 'Gateway request failed.'));
      else entry.resolve(message.result);
    };
  });
}

function disconnectGateway() {
  ++socketGeneration;
  socket?.close();
  socket = null;
  rejectRpc('Gateway disconnected.');
  updateConnection('offline');
}

function setTab(tab) {
  state.tab = tab;
  for (const name of ['conversation', 'activity', 'approvals', 'studio']) {
    const selected = tab === name;
    $(`tab-${name}`).classList.toggle('active', selected);
    $(`tab-${name}`).setAttribute('aria-selected', String(selected));
    $(`tab-${name}`).tabIndex = selected ? 0 : -1;
    $(`${name}-panel`).hidden = !selected;
  }
  $('form').hidden = tab !== 'conversation';
  if (tab === 'studio' && !state.studioLoaded) void loadStudio();
}

function closeDrawers() {
  $('session-rail').classList.remove('open');
  $('inspector').classList.remove('open');
  $('drawer-backdrop').hidden = true;
  $('sessions-toggle').setAttribute('aria-expanded', 'false');
  if (matchMedia('(max-width: 1170px)').matches) $('inspector-toggle').setAttribute('aria-expanded', 'false');
}

function toggleDrawer(kind) {
  if (kind === 'inspector' && !matchMedia('(max-width: 1170px)').matches) {
    const hidden = document.querySelector('.shell').classList.toggle('inspector-hidden');
    $('inspector-toggle').setAttribute('aria-expanded', String(!hidden));
    return;
  }
  const element = kind === 'sessions' ? $('session-rail') : $('inspector');
  const open = !element.classList.contains('open');
  closeDrawers();
  if (open) {
    element.classList.add('open');
    $('drawer-backdrop').hidden = false;
    $(`${kind === 'sessions' ? 'sessions' : 'inspector'}-toggle`).setAttribute('aria-expanded', 'true');
  }
}

function selectedMeta() { return state.sessions.find((s) => s.id === state.selectedSessionId); }

function safeMediaUrl(id) {
  return mediaPath(id) || '';
}

async function hydrateMediaSource(source) {
  const path = mediaPath(source);
  if (!path) return safeImageSource(source);
  if (state.mediaObjectUrls.has(path)) return state.mediaObjectUrls.get(path);
  const response = await fetch(path, { headers: authHeaders() });
  if (!response.ok) throw new Error(`Media request failed with HTTP ${response.status}.`);
  const type = rasterMediaType(response.headers.get('Content-Type'));
  if (!type) throw new Error('The media response was not a raster image.');
  const url = URL.createObjectURL(await response.blob());
  state.mediaObjectUrls.set(path, url);
  return url;
}

async function hydrateMessageImages(message) {
  const sources = message._mediaSources || messageImageSources(message);
  if (!sources.length) return [];
  return Promise.all(sources.map(async (source) => {
    const id = source.match(/^\/api\/media\/([^/]+)$/i)?.[1];
    if (!id) return source;
    try { return await hydrateMediaSource(id); } catch { return ''; }
  })).then((items) => items.filter(Boolean));
}

async function hydrateTranscriptImages(messages) {
  const version = state.transcriptVersion;
  const hydrated = await Promise.all(messages.map(async (message) => ({ message, sources: await hydrateMessageImages(message) })));
  if (version !== state.transcriptVersion) return;
  for (const { message, sources } of hydrated) message._mediaSources = sources;
  renderTranscript(messages);
}


function renderIdentity() {
  const meta = selectedMeta();
  $('session-title').textContent = meta?.title && meta.title !== 'new session' ? meta.title : 'New session';
  $('session-subtitle').textContent = meta?.id ? `${meta.id.slice(0, 8)} / ${meta.model || state.status?.model || 'Model'}` : 'Local workspace';
  const paused = state.paused.get(state.selectedSessionId) === true;
  const pending = [...approvals.pending.values()].some((a) => a.session === state.selectedSessionId);
  const running = state.status?.busy && state.selectedSessionId === state.serverSessionId;
  $('runtime-state').textContent = paused ? 'Paused' : pending ? 'Needs approval' : running ? 'Running' : 'Idle';
  $('runtime-state').className = `runtime-state ${pending ? 'waiting' : running ? 'running' : ''}`;
  $('pause-session').setAttribute('aria-label', paused ? 'Resume session' : 'Pause session');
  $('pause-session').title = paused ? 'Resume session' : 'Pause at the next model boundary';
  $('pause-icon').setAttribute('href', `/assets/icons.svg#${paused ? 'play' : 'pause'}`);
}

function renderControls() {
  const hasSession = Boolean(state.selectedSessionId);
  const paused = state.paused.get(state.selectedSessionId) === true;
  const runtimeBusy = state.status?.busy === true;
  const pendingAttachments = state.attachments.some((attachment) => attachment.uploading);
  $('send').disabled = !state.status || state.sending || paused || runtimeBusy || pendingAttachments || !$('input').value.trim();
  $('attachment-picker').classList.toggle('disabled', state.sending || runtimeBusy);
  $('attachment-picker').setAttribute('aria-disabled', String(state.sending || runtimeBusy));
  $('new-session').disabled = state.sending || runtimeBusy || !state.status;
  $('model').disabled = state.sending || runtimeBusy || !state.modelsLoaded || !connected();
  $('pause-session').disabled = !hasSession || !connected();
  $('context-open').disabled = !hasSession || !connected() || state.sending || runtimeBusy;
  $('composer-status').textContent = state.sending ? 'Running' : paused ? 'Paused' : runtimeBusy ? 'Busy' : state.attachments.length ? `${state.attachments.length} attached` : 'Ready';
  $('send').querySelector('span').textContent = state.sending ? 'Running' : 'Send';
  renderAttachmentChips();
  renderIdentity();
}

function renderStatus(status) {
  state.status = status;
  state.serverSessionId = status.session || null;
  state.selectedSessionId = selectedAfterRefresh(state.selectedSessionId, state.serverSessionId);
  if (typeof status.paused === 'boolean' && state.serverSessionId) state.paused.set(state.serverSessionId, status.paused);
  $('workspace-path').textContent = status.cwd || 'Local workspace';
  $('workspace-path').title = status.cwd || '';
  const data = $('runtime-data');
  data.replaceChildren();
  for (const [label, value] of [
    ['Provider', status.provider], ['Model', status.model], ['Effort', status.effort],
    ['Sandbox', status.sandbox], ['Permission', status.permission], ['Turns', status.turns],
  ]) {
    const row = node('div');
    row.append(node('dt', '', label), node('dd', '', String(value ?? '—')));
    data.append(row);
  }
  const usage = status.usage || {};
  $('usage-source').textContent = usage.reported ? 'Reported' : 'Not reported';
  $('usage-input').textContent = usage.reported ? formatNumber(usage.input_tokens) : '—';
  $('usage-output').textContent = usage.reported ? formatNumber(usage.output_tokens) : '—';
  $('usage-total').textContent = usage.reported ? formatNumber(usage.total_tokens) : '—';
  $('context-estimate').textContent = usage.context_estimate_tokens == null ? '—' : `~${formatNumber(usage.context_estimate_tokens)} tokens`;
  const goal = status.goal;
  $('goal-section').hidden = !goal;
  if (goal) {
    $('goal-status').textContent = goal.status;
    $('goal-status').className = `state-label ${goal.status === 'met' ? 'good' : goal.status === 'active' ? 'waiting' : ''}`;
    $('goal-text').textContent = goal.condition;
    $('goal-meta').replaceChildren(node('span', '', `${goal.rounds}/${goal.max_rounds} rounds`), node('span', '', formatDuration(goal.elapsed_secs)));
    $('goal-verdict').hidden = !goal.last_verdict;
    $('goal-verdict').textContent = goal.last_verdict?.reason || '';
  }
  $('skills').replaceChildren();
  $('skill-count').textContent = String(status.skills?.length || 0);
  const skillNames = status.skills || [];
  for (const name of state.skillsExpanded ? skillNames : skillNames.slice(0, 8)) {
    const button = node('button', 'skill-button', `/${name}`);
    button.type = 'button';
    button.addEventListener('click', () => { $('input').value = `/${name} `; setTab('conversation'); $('input').focus(); renderControls(); closeDrawers(); });
    $('skills').append(button);
  }
  if (skillNames.length > 8) {
    const expand = node('button', 'skill-button', state.skillsExpanded ? 'Show fewer' : `Show all ${skillNames.length}`);
    expand.type = 'button';
    expand.addEventListener('click', () => { state.skillsExpanded = !state.skillsExpanded; renderStatus(state.status); });
    $('skills').append(expand);
  }
  if (!status.skills?.length) $('skills').append(node('span', 'muted', 'No skills loaded'));
  if (state.modelsLoaded && [...$('model').options].some((option) => option.value === status.model)) $('model').value = status.model;
  renderControls();
}

function renderSessions() {
  $('sessions').replaceChildren();
  const list = state.sessions.filter((s) => matchesSessionQuery(s, $('session-search').value));
  for (const session of list) {
    const button = node('button', `session-item${session.id === state.selectedSessionId ? ' active' : ''}`);
    button.type = 'button';
    button.setAttribute('aria-current', session.id === state.selectedSessionId ? 'true' : 'false');
    button.title = session.title || session.id;
    button.append(node('span', 'session-title', session.title === 'new session' ? 'New session' : session.title || session.id));
    const meta = node('span', 'session-meta');
    meta.append(node('span', '', relativeTime(session.updated_at)), node('span', 'unread', session.unread ? `${session.unread} unread` : session.model || ''));
    button.append(meta);
    button.addEventListener('click', () => { void loadSession(session.id).catch((error) => setError($('page-error'), errorText(error))); });
    $('sessions').append(button);
  }
  if (!list.length) $('sessions').append(node('div', 'quiet-empty', state.sessions.length ? 'No matching sessions.' : 'No saved sessions.'));
  renderIdentity();
}

function appendImageGallery(content, sources, label) {
  const valid = sources.map(safeImageSource).filter((source) => source && !source.startsWith('/api/media/'));
  if (!valid.length) return;
  const gallery = content.querySelector('.message-images') || node('div', 'message-images');
  const existing = new Set([...gallery.querySelectorAll('img')].map((image) => image.src));
  for (const source of valid) {
    if (existing.has(source)) continue;
    const image = document.createElement('img');
    image.src = source;
    image.alt = `${label} attachment`;
    image.loading = 'lazy';
    gallery.append(image);
    existing.add(source);
  }
  if (!gallery.parentNode) content.append(gallery);
}

function renderMessage(role, text, live = false, message = null) {
  const article = node('article', 'message');
  const label = role === 'assistant' ? 'Varynth' : role === 'user' ? 'You' : role === 'error' ? 'Error' : role === 'tool' ? 'Tool result' : 'System';
  const avatar = node('div', `avatar ${role}`, role === 'assistant' ? 'V' : role === 'user' ? 'Y' : role === 'error' ? '!' : 'S');
  avatar.setAttribute('aria-hidden', 'true');
  const content = node('div');
  const heading = node('div', 'message-header');
  heading.append(node('span', '', label));
  const phase = node('span', 'message-state', live ? 'Responding…' : '');
  heading.append(phase);
  const body = node('div', 'message-text');
  setMessageText(body, text, live);
  content.append(heading, body);
  const sources = message?._mediaSources || messageImageSources(message || { role, content: text });
  appendImageGallery(content, sources, label);
  article.append(avatar, content);
  return { article, body, phase, content };
}

function setMessageText(body, text, live = false) {
  body.replaceChildren();
  if (live) { body.textContent = text; return; }
  for (const part of messageParts(text)) {
    if (part.kind === 'code') {
      const pre = node('pre', 'code-block');
      pre.append(node('code', '', part.text));
      body.append(pre);
    } else {
      body.append(document.createTextNode(part.text));
    }
  }
}

function renderTranscript(messages) {
  $('log').replaceChildren();
  if (!messages.length) {
    const empty = node('div', 'conversation-empty');
    const mark = document.createElement('img');
    mark.src = '/assets/mark.svg'; mark.alt = ''; mark.width = 40; mark.height = 40;
    empty.append(mark, node('h2', '', 'New session'), node('p', '', 'No messages yet.'));
    $('log').append(empty);
  } else {
    for (const message of messages) {
      if (message.role === 'tool') continue;
      const rendered = renderMessage(message.role, message.content || '', false, message);
      if (message.tool_calls?.length && !message.content) rendered.body.textContent = message.tool_calls.map((call) => `Calling ${call.name}`).join('\n');
      $('log').append(rendered.article);
    }
  }
  $('conversation-panel').scrollTop = $('conversation-panel').scrollHeight;
}

async function loadSession(id, force = false) {
  if (state.sending && !force) { toast('Wait for the current turn before switching sessions.'); return; }
  invalidateAttachments();
  const version = ++state.transcriptVersion;
  setError($('page-error'));
  const result = assertOk(await api(`/api/session/${encodeURIComponent(id)}`));
  if (version !== state.transcriptVersion) return;
  state.selectedSessionId = result.meta.id;
  state.messages = result.messages || [];
  if (typeof result.paused === 'boolean') state.paused.set(id, result.paused);
  renderTranscript(state.messages);
  void hydrateTranscriptImages(state.messages);
  const known = state.sessions.find((s) => s.id === result.meta.id);
  if (known) Object.assign(known, result.meta, { unread: result.unread });
  else state.sessions.unshift({ ...result.meta, unread: result.unread });
  renderSessions();
  renderControls();
  setTab('conversation');
  closeDrawers();
}

async function refresh({ transcript = false } = {}) {
  if (state.refreshRunning) return;
  state.refreshRunning = true;
  try {
    const [status, sessions] = await Promise.all([api('/api/status'), api('/api/sessions')]);
    renderStatus(status);
    state.sessions = sessions.sessions || [];
    renderSessions();
    if (transcript && state.selectedSessionId && !state.sending) await loadSession(state.selectedSessionId, true);
  } catch (error) {
    setError($('page-error'), errorText(error));
    if (!state.status) $('send').disabled = true;
  } finally { state.refreshRunning = false; }
}

async function loadModels() {
  if (modelsPromise) return modelsPromise;
  modelsPromise = (async () => {
    const result = assertOk(await api('/api/models'));
    const select = $('model');
    select.replaceChildren();
    for (const model of result.models || []) {
      select.append(new Option(model.display_name || model.id, model.id));
    }
    if (state.status?.model && ![...select.options].some((option) => option.value === state.status.model)) {
      select.prepend(new Option(state.status.model, state.status.model));
    }
    if (!select.options.length) select.append(new Option('No models available', ''));
    else select.value = state.status?.model || select.options[0].value;
    state.modelsLoaded = true;
    renderControls();
  })();
  try { await modelsPromise; } finally { modelsPromise = null; }
}

function addEvent(kind, text, session, timestamp = new Date().toISOString()) {
  if (kind === 'delta') return;
  state.events.push({ kind, text: String(text).slice(0, 3000), session, timestamp });
  if (state.events.length > 200) state.events.shift();
  $('activity-count').textContent = String(state.events.length);
  renderEvents();
}

function renderEvents() {
  const filtered = state.events.filter((event) => eventMatchesFilter(event.kind, $('event-filter').value));
  $('events').replaceChildren();
  if (!filtered.length) { $('events').append(node('div', 'quiet-empty', 'No events in this view.')); return; }
  for (const event of filtered) {
    const row = node('div', 'event-row');
    const date = new Date(event.timestamp);
    const time = node('time', 'event-time', Number.isNaN(date.getTime()) ? '' : date.toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' }));
    const kind = node('span', `event-kind ${event.kind.replace(/[^a-z_]/g, '')}`, event.kind);
    const detail = node('div', 'event-detail', event.text);
    if (event.session) detail.append(node('div', 'event-session', event.session.slice(0, 8)));
    row.append(time, kind, detail);
    $('events').append(row);
  }
}

function renderApprovals() {
  const requests = [...approvals.pending.values()];
  $('approval-count').textContent = String(requests.length);
  $('approval-count').classList.toggle('pending', requests.length > 0);
  $('approval-summary').textContent = requests.length ? `${requests.length} pending` : 'No pending requests';
  $('approvals').replaceChildren();
  for (const request of requests) {
    const card = node('article', 'approval-card');
    const heading = node('div', 'approval-heading');
    heading.append(icon('shield-check'), node('span', '', request.tool));
    const detail = node('div', 'approval-detail', request.detail);
    const meta = node('div', 'approval-meta');
    meta.append(node('span', '', `Session ${request.session.slice(0, 8) || 'unknown'}`));
    const copy = node('button', 'icon-button');
    copy.type = 'button'; copy.title = 'Copy approval id'; copy.setAttribute('aria-label', 'Copy approval id');
    copy.append(icon('copy'));
    copy.addEventListener('click', async () => {
      try { await navigator.clipboard.writeText(request.id); toast('Approval id copied'); }
      catch { toast('Clipboard is unavailable.'); }
    });
    meta.append(copy);
    const actions = node('div', 'approval-actions');
    for (const [decision, label, style] of [['once', 'Allow once', 'primary'], ['always', 'Always allow', 'subtle'], ['deny', 'Deny', 'danger']]) {
      const button = node('button', `button ${style}`, request.sending ? 'Sending…' : label);
      button.type = 'button'; button.disabled = request.sending || !connected();
      button.addEventListener('click', () => { void respondApproval(request, decision); });
      actions.append(button);
    }
    card.append(heading, detail, meta, actions);
    if (request.error) card.append(node('div', 'approval-error', request.error));
    $('approvals').append(card);
  }
  if (!requests.length) $('approvals').append(node('div', 'quiet-empty', 'No pending requests.'));
  renderControls();
}

async function respondApproval(request, decision) {
  request.sending = true;
  request.error = '';
  renderApprovals();
  try {
    const result = await rpc('approval.respond', { id: request.id, decision });
    if (result === true) {
      approvals.resolve(request.id);
      toast(decision === 'deny' ? 'Tool request denied' : 'Tool request approved');
      addEvent('system', `Approval ${decision} accepted`, request.session);
    } else if (result === false) {
      approvals.resolve(request.id);
      toast('This approval is no longer pending.');
    } else { throw new Error('The gateway returned an invalid approval response.'); }
  } catch (error) { request.error = errorText(error); }
  finally { request.sending = false; renderApprovals(); }
}

function handleEvent(event, session, timestamp, source) {
  if (event?.kind === 'image') return;
  const request = normalizeApproval(event, session);
  if (request) { approvals.add(request); renderApprovals(); }
  if (event.kind === 'approval_request' && !request) return;
  if (source === 'gateway' && state.stream?.session === session) return;
  addEvent(event.kind, event.text, session, timestamp);
  if (request && state.selectedSessionId === session) renderIdentity();
}

function handleGatewayEvent(frame) {
  if (!frame) return;
  if (frame.type === 'lagged') {
    addEvent('system', frame.text || `${frame.missed} events skipped`, state.serverSessionId);
    void refresh();
    return;
  }
  if (frame.type !== 'event' || !frame.event) return;
  handleEvent(frame.event, frame.session, frame.ts, 'gateway');
  if (['system', 'approval_pending', 'assistant', 'error'].includes(frame.event.kind)) scheduleRefresh();
}

function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => { void refresh(); }, 500);
}

function renderAttachmentChips() {
  const container = $('attachment-chips');
  container.replaceChildren();
  for (const attachment of state.attachments) {
    const chip = node('div', `attachment-chip ${attachment.error ? 'has-error' : ''}`);
    const preview = document.createElement('img');
    preview.src = attachment.preview || safeImageSource(attachment.url) || '';
    preview.alt = attachment.name;
    preview.loading = 'lazy';
    chip.append(preview);
    const label = node('span', 'attachment-name', attachment.error || attachment.name);
    chip.append(label);
    if (attachment.uploading) chip.append(node('span', 'attachment-state', 'Uploading…'));
    const remove = node('button', 'icon-button', '');
    remove.type = 'button';
    remove.setAttribute('aria-label', `Remove ${attachment.name}`);
    remove.title = 'Remove attachment';
    remove.append(icon('x'));
    remove.addEventListener('click', () => removeAttachment(attachment));
    chip.append(remove);
    container.append(chip);
  }
}

function invalidateAttachments() {
  ++state.attachmentGeneration;
  for (const attachment of state.attachments) {
    attachment.removed = true;
    if (attachment.preview?.startsWith('blob:')) URL.revokeObjectURL(attachment.preview);
  }
  state.attachments = [];
  renderControls();
}

function removeAttachment(attachment) {
  const index = state.attachments.indexOf(attachment);
  if (index < 0) return;
  attachment.removed = true;
  attachment.generation = ++state.attachmentGeneration;
  state.attachments.splice(index, 1);
  if (attachment.preview?.startsWith('blob:')) URL.revokeObjectURL(attachment.preview);
  renderControls();
}

function mediaResult(result, file) {
  const media = result?.media || result?.attachment || result;
  const id = media?.id || media?.media_id || media?.mediaId;
  if (!id || !mediaPath(id)) throw new Error('Media upload did not return a valid attachment id.');
  return { id: String(id), url: mediaPath(id), name: file.name, type: rasterMediaType(file.type) };
}

async function uploadAttachment(file) {
  const type = rasterMediaType(file?.type);
  if (!(file instanceof File) || !type) throw new Error('Only PNG, JPEG, WebP, or GIF images can be attached.');
  if (file.size > 8 * 1024 * 1024) throw new Error('Each image must be 8 MiB or smaller.');
  if (state.attachments.length >= 8) throw new Error('Attach up to 8 images per message.');
  const attachment = {
    name: file.name || 'Image',
    size: file.size,
    type,
    session: state.selectedSessionId,
    preview: URL.createObjectURL(file),
    uploading: true,
    error: '',
    removed: false,
    generation: ++state.attachmentGeneration,
    token: state.token,
  };
  const generation = attachment.generation;
  const token = attachment.token;
  state.attachments.push(attachment);
  renderControls();
  try {
    const result = await api('/api/media', {
      method: 'POST',
      headers: { 'Content-Type': type, 'x-media-type': type },
      body: file,
    });
    if (attachment.removed || attachment.session !== state.selectedSessionId || attachment.generation !== generation || attachment.token !== state.token || !state.attachments.includes(attachment)) return;
    Object.assign(attachment, mediaResult(result, file), { uploading: false });
  } catch (error) {
    if (attachment.removed || attachment.session !== state.selectedSessionId || attachment.generation !== generation || attachment.token !== state.token || !state.attachments.includes(attachment)) return;
    attachment.uploading = false;
    attachment.error = errorText(error);
  }
  renderControls();
}

async function addFiles(files) {
  const images = [...files].filter((file) => file instanceof File && rasterMediaType(file.type));
  if (state.sending || state.status?.busy) { toast('Wait for the current turn to finish.'); return; }
  if (!images.length && files.length) toast('Choose a PNG, JPEG, WebP, or GIF image.');
  if (images.length + state.attachments.length > 8) toast('Attach up to 8 images per message.');
  const totalBytes = state.attachments.reduce((sum, item) => sum + (item.size || 0), 0) + images.reduce((sum, file) => sum + file.size, 0);
  if (totalBytes > 16 * 1024 * 1024) { toast('Attachments must total 16 MiB or less.'); return; }
  for (const file of images.slice(0, Math.max(0, 8 - state.attachments.length))) {
    try { await uploadAttachment(file); } catch (error) { toast(errorText(error)); }
  }
}

function attachmentIds() {
  return state.attachments.filter((attachment) => attachment.id && !attachment.uploading && !attachment.error).map((attachment) => attachment.id);
}

function attachmentMetadata() {
  return state.attachments.filter((attachment) => !attachment.error).map((attachment) => ({
    url: attachment.url || attachment.preview,
    name: attachment.name,
  }));
}

async function sendMessage(event) {
  event.preventDefault();
  if (state.sending || !state.status || state.status.busy || state.paused.get(state.selectedSessionId)) return;
  const text = $('input').value.trim();
  if (!text || state.attachments.some((attachment) => attachment.error)) {
    if (!text) toast('Add a message before sending.');
    else toast('Remove failed attachments before sending.');
    return;
  }
  if (state.attachments.some((attachment) => attachment.uploading)) return;
  if (state.attachments.reduce((sum, attachment) => sum + (attachment.size || 0), 0) > 16 * 1024 * 1024) { toast('Attachments must total 16 MiB or less.'); return; }
  const session = state.selectedSessionId;
  const version = state.transcriptVersion;
  const mediaIds = attachmentIds();
  const attached = state.attachments.filter((attachment) => mediaIds.includes(attachment.id));
  setError($('page-error'));
  const message = { role: 'user', content: text, metadata: { images: attachmentMetadata() }, _mediaSources: attached.map((attachment) => attachment.preview) };
  state.messages.push(message);
  renderTranscript(state.messages);
  $('input').value = '';
  state.sending = true;
  state.stream = { session, version, text: '', images: [], response: null, finalSeen: false, done: false };
  renderControls();
  let accepted = false;
  try {
    const response = await fetch('/api/chat/stream', {
      method: 'POST',
      headers: authHeaders({ 'Content-Type': 'application/json', Accept: 'text/event-stream' }),
      body: JSON.stringify({ message: text, session_id: session, model: $('model').value || state.status.model, media_ids: mediaIds }),
    });
    if (response.status === 401) throw new Error('Authentication required. Add the dashboard token in Connection.');
    if (!response.ok) throw new Error(`Turn request failed with HTTP ${response.status}.`);
    if (!response.headers.get('Content-Type')?.includes('text/event-stream') || !response.body) {
      throw new Error('The server did not return a response stream.');
    }
    accepted = true;
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    const sse = new SseDecoder();
    const stream = state.stream;
    const updateAnswer = (value, final = false) => {
      if (!stream.response) {
        stream.response = renderMessage('assistant', '', true);
        $('log').append(stream.response.article);
      }
      stream.text = value;
      setMessageText(stream.response.body, value, !final);
      appendImageGallery(stream.response.content, stream.images, 'Varynth');
      stream.response.phase.textContent = final ? '' : 'Responding…';
      $('conversation-panel').scrollTop = $('conversation-panel').scrollHeight;
    };
    const consume = (frame) => {
      if (frame.kind === 'error') throw new Error(frame.text);
      if (frame.kind === 'session') { stream.session = frame.text; state.selectedSessionId = frame.text; return; }
      if (frame.kind === 'delta') { updateAnswer(stream.text + frame.text); return; }
      if (frame.kind === 'assistant') { updateAnswer(frame.text, true); stream.finalSeen = true; return; }
      if (frame.kind === 'done') { updateAnswer(frame.text, true); stream.done = true; return; }
      if (frame.kind === 'image') {
        const source = parseImageEvent(frame.text);
        if (source) {
          stream.images.push(source);
          if (stream.response) appendImageGallery(stream.response.content, [source], 'Varynth');
        }
        return;
      }
      handleEvent(frame, stream.session, undefined, 'stream');
    };
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      for (const frame of sse.feed(decoder.decode(value, { stream: true }))) consume(frame);
    }
    for (const frame of sse.feed(decoder.decode())) consume(frame);
    sse.finish();
    if (!stream.done) throw new Error('The response stream ended before the turn completed.');
    state.messages.push({ role: 'assistant', content: stream.text, images: stream.images.map((source) => ({ data_url: source })), _mediaSources: stream.images });
    state.attachments = state.attachments.filter((attachment) => !attached.includes(attachment));
    for (const attachment of attached) if (attachment.preview?.startsWith('blob:')) URL.revokeObjectURL(attachment.preview);
  } catch (error) {
    setError($('page-error'), errorText(error));
    if (!accepted) { state.messages.pop(); $('input').value = text; renderTranscript(state.messages); }
    else if (state.stream?.response) state.stream.response.phase.textContent = 'Interrupted';
  } finally {
    state.sending = false;
    state.stream = null;
    renderControls();
    await refresh({ transcript: accepted });
    if (accepted && session === state.selectedSessionId) await loadSession(session, true);
  }
}

function studioValue(value, fallback = '—') {
  if (value == null || value === '') return fallback;
  return String(value);
}

function studioRows(value) {
  if (Array.isArray(value)) return value;
  if (value && typeof value === 'object') {
    if (value.id || value.name || value.hint || value.target) return [value];
    return Object.entries(value).map(([id, item]) => ({ id, ...(item && typeof item === 'object' ? item : { name: item }) }));
  }
  return [];
}

function fillProviderForm(provider, profile) {
  const values = {
    'studio-profile-id': profile?.id || '',
    'studio-profile-kind': profile?.kind || provider?.name || '',
    'studio-profile-base-url': profile?.base_url || profile?.baseUrl || '',
    'studio-profile-model': profile?.models?.[0] || profile?.model || provider?.model || '',
    'studio-profile-api-key-env': profile?.api_key_env || profile?.apiKeyEnv || '',
    'studio-profile-auth': profile?.auth || '',
    'studio-profile-project': profile?.project || '',
    'studio-profile-location': profile?.location || '',
  };
  for (const [id, value] of Object.entries(values)) $(id).value = value;
}

function ensureModelOption(model) {
  if (!model) return;
  const select = $('model');
  if (![...select.options].some((option) => option.value === model)) select.append(new Option(model, model));
  select.value = model;
}
function renderStudio(result) {
  state.studio = result || {};
  const provider = result?.provider || {};
  const profile = provider.profile || {};
  const profiles = Array.isArray(provider.profiles) ? provider.profiles : [];
  $('studio-provider').replaceChildren();
  for (const [label, value] of [
    ['Provider', provider.name],
    ['Profile', profile.name || profile.id],
    ['Model', provider.model],
    ['Status', provider.status],
  ]) {
    const row = node('div');
    row.append(node('dt', '', label), node('dd', '', studioValue(value)));
    $('studio-provider').append(row);
  }
  $('studio-provider-state').textContent = provider.status || 'Loaded';
  $('studio-provider-state').className = `state-label ${provider.status === 'error' ? 'bad' : provider.status === 'ready' || provider.status === 'connected' ? 'good' : ''}`;
  const profileSelect = $('studio-profile-select');
  profileSelect.replaceChildren(new Option('Current profile', profile.id || ''));
  for (const item of profiles) profileSelect.append(new Option(item.name || item.id, item.id));
  profileSelect.value = profile.id || '';
  for (const item of profiles) {
    if (item.id === profile.id) ensureModelOption(item.models?.[0]);
  }
  ensureModelOption(profile.models?.[0] || provider.model);
  fillProviderForm(provider, profile);

  const servers = studioRows(result?.mcp);
  $('studio-mcp-count').textContent = String(servers.length);
  $('studio-mcp').replaceChildren();
  for (const server of servers) {
    const row = node('div', 'studio-row');
    const detail = node('div', 'studio-row-detail');
    detail.append(node('strong', '', studioValue(server.name, 'MCP server')), node('span', '', `${studioValue(server.status, 'Unknown')} · ${studioValue(server.transport, 'HTTP')} · ${studioValue(server.target, '—')} · ${studioValue(server.tool_count, '0')} tools`));
    const probe = node('button', 'button subtle studio-row-action', 'Probe');
    probe.type = 'button';
    probe.addEventListener('click', () => { void probeMcp(server, probe); });
    row.append(detail, probe);
    $('studio-mcp').append(row);
  }
  if (!servers.length) $('studio-mcp').append(node('div', 'quiet-empty', 'No MCP servers returned.'));

  const skills = studioRows(result?.skills);
  $('studio-skill-count').textContent = String(skills.length);
  $('studio-skills').replaceChildren();
  for (const skill of skills) $('studio-skills').append(node('span', 'studio-chip', `/${studioValue(skill.name || skill.id || skill, 'skill')}`));
  if (!skills.length) $('studio-skills').append(node('span', 'muted', 'No installed skills returned.'));

  const plugins = studioRows(result?.plugins);
  $('studio-plugin-count').textContent = String(plugins.length);
  $('studio-plugins').replaceChildren();
  for (const plugin of plugins) {
    const row = node('div', 'studio-row');
    const detail = node('div', 'studio-row-detail');
    detail.append(node('strong', '', studioValue(plugin.name, 'Plugin')), node('span', '', studioValue(plugin.hint, 'Curated integration')));
    const installed = plugin.installed === true;
    const action = node('button', `button ${installed ? 'danger' : 'subtle'} studio-row-action`, installed ? 'Remove' : 'Install');
    action.type = 'button';
    action.addEventListener('click', () => { void mutatePlugin(plugin, installed, action); });
    row.append(detail, action);
    $('studio-plugins').append(row);
  }
  if (!plugins.length) $('studio-plugins').append(node('div', 'quiet-empty', 'No curated plugins returned.'));
}

async function saveProviderProfile(event) {
  event.preventDefault();
  setError($('studio-provider-error'));
  const model = $('studio-profile-model').value.trim();
  const id = $('studio-profile-id').value.trim();
  const kind = $('studio-profile-kind').value.trim();
  if (!model || !id || !kind) { setError($('studio-provider-error'), 'Profile id, kind, and model are required.'); return; }
  const body = {
    id,
    kind,
    base_url: $('studio-profile-base-url').value.trim(),
    models: [model],
    api_key_env: $('studio-profile-api-key-env').value.trim(),
    auth: $('studio-profile-auth').value.trim(),
    project: $('studio-profile-project').value.trim(),
    location: $('studio-profile-location').value.trim(),
  };
  $('studio-provider-form').querySelector('button[type="submit"]').disabled = true;
  try {
    await api('/api/studio/provider', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    state.studioLoaded = false;
    await loadStudio();
    ensureModelOption(model);
    toast('Provider profile saved');
  } catch (error) { setError($('studio-provider-error'), errorText(error)); }
  finally { $('studio-provider-form').querySelector('button[type="submit"]').disabled = false; }
}

async function addMcpServer(event) {
  event.preventDefault();
  setError($('studio-mcp-error'));
  const id = $('studio-mcp-id').value.trim();
  const url = $('studio-mcp-url').value.trim();
  if (!id || !/^https?:\/\/[^\s]+$/i.test(url)) { setError($('studio-mcp-error'), 'Enter an id and HTTP URL.'); return; }
  const button = $('studio-mcp-form').querySelector('button[type="submit"]');
  button.disabled = true;
  try {
    await api('/api/studio/mcp', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ id, url }) });
    $('studio-mcp-id').value = '';
    $('studio-mcp-url').value = '';
    state.studioLoaded = false;
    await loadStudio();
  } catch (error) { setError($('studio-mcp-error'), errorText(error)); }
  finally { button.disabled = false; }
}

async function installSkill(event) {
  event.preventDefault();
  setError($('studio-skill-error'));
  let source;
  try { source = validateSkillSource($('studio-skill-source').value); }
  catch (error) { setError($('studio-skill-error'), errorText(error)); return; }
  const button = $('studio-skill-form').querySelector('button[type="submit"]');
  button.disabled = true;
  try {
    await api('/api/studio/skill', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ source }) });
    $('studio-skill-source').value = '';
    state.studioLoaded = false;
    await loadStudio();
  } catch (error) { setError($('studio-skill-error'), errorText(error)); }
  finally { button.disabled = false; }
}
async function loadStudio() {
  const version = ++state.studioRequestVersion;
  $('studio-refresh').disabled = true;
  setError($('studio-error'));
  try {
    const result = await api('/api/studio');
    if (version !== state.studioRequestVersion) return;
    renderStudio(result);
    state.studioLoaded = true;
  } catch (error) {
    if (version !== state.studioRequestVersion) return;
    state.studioLoaded = false;
    setError($('studio-error'), `Studio unavailable: ${errorText(error)}`);
  } finally { if (version === state.studioRequestVersion) $('studio-refresh').disabled = false; }
}



async function probeMcp(server, button) {
  const old = button.textContent;
  button.disabled = true;
  button.textContent = 'Probing…';
  try {
    await api('/api/studio/mcp/probe', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ id: server.name }) });
    state.studioLoaded = false;
    await loadStudio();
    toast(`MCP ${server.name || 'server'} probed`);
  } catch (error) { button.textContent = old; toast(errorText(error)); }
  finally { button.disabled = false; }
}

async function mutatePlugin(plugin, installed, button) {
  if (state.studioMutation) return;
  state.studioMutation = true;
  button.disabled = true;
  const action = installed ? 'remove' : 'install';
  try {
    await api('/api/studio/plugin', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ action, id: plugin.id }) });
    state.studioLoaded = false;
    await loadStudio();
    toast(`${plugin.name || plugin.id || 'Plugin'} ${action === 'install' ? 'installed' : 'removed'}`);
  } catch (error) { toast(errorText(error)); }
  finally { state.studioMutation = false; button.disabled = false; }
}

function applyTheme(tokens) {
  for (const name of THEME_TOKENS) {
    if (tokens[name]) document.documentElement.style.setProperty(name, tokens[name]);
  }
  state.themeTokens = { ...state.themeTokens, ...tokens };
  $('theme-input').value = themeText(state.themeTokens);
  $('theme-state').className = 'state-label good';
}

function themeText(tokens) {
  return JSON.stringify(tokens, null, 2);
}

async function loadTheme() {
  $('theme-load').disabled = true;
  setError($('theme-error'));
  try {
    const result = await api('/api/theme');
    const tokens = validateThemeTokens(result?.tokens || {});
    state.themeTokens = tokens;
    $('theme-input').value = themeText(tokens);
    applyTheme(tokens);
  } catch (error) {
    setError($('theme-error'), `Theme unavailable: ${errorText(error)}`);
  } finally { $('theme-load').disabled = false; }
}

function readThemeInput() {
  const tokens = parseThemeInput($('theme-input').value);
  applyTheme(tokens);
  $('theme-input').value = themeText(state.themeTokens);
  setError($('theme-error'));
  return tokens;
}

async function saveTheme() {
  $('theme-save').disabled = true;
  try {
    const tokens = readThemeInput();
    const result = await api('/api/theme', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ tokens }) });
    const canonical = validateThemeTokens(result?.tokens || tokens);
    applyTheme(canonical);
    $('theme-state').textContent = 'Saved';
    toast('Theme saved');
  } catch (error) { setError($('theme-error'), errorText(error)); }
  finally { $('theme-save').disabled = false; }
}

function exportTheme() {
  try {
    const tokens = readThemeInput();
    const blob = new Blob([themeText(tokens)], { type: 'application/json' });
    const link = document.createElement('a');
    link.href = URL.createObjectURL(blob);
    link.download = 'varynth-theme.json';
    link.click();
    setTimeout(() => URL.revokeObjectURL(link.href), 0);
  } catch (error) { setError($('theme-error'), errorText(error)); }
}


async function newSession() {
  $('new-session').disabled = true;
  invalidateAttachments();
  setError($('page-error'));
  try {
    const result = assertOk(await api('/api/session/new', { method: 'POST' }));
    state.selectedSessionId = result.session_id;
    state.messages = [];
    ++state.transcriptVersion;
    renderTranscript([]);
    setTab('conversation');
    await refresh();
    closeDrawers();
    $('input').focus();
  } catch (error) { setError($('page-error'), errorText(error)); }
  finally { renderControls(); }
}

async function healthChecks() {
  $('check-health').disabled = true;
  try {
    const result = await api('/api/doctor');
    $('doctor').replaceChildren();
    for (const check of result.checks || []) {
      const row = node('div', `health-row ${check.ok ? 'good' : 'bad'}`);
      row.append(icon(check.ok ? 'check' : 'circle-alert'));
      const description = node('div');
      description.append(node('b', '', check.name), node('span', '', check.detail));
      row.append(description);
      $('doctor').append(row);
    }
    if (!result.checks?.length) $('doctor').append(node('span', 'muted', 'No checks returned'));
  } catch (error) { $('doctor').replaceChildren(node('span', 'muted', errorText(error))); }
  finally { $('check-health').disabled = false; }
}

async function channels() {
  try {
    const result = await api('/api/channels');
    $('channels').replaceChildren();
    for (const [key, value] of Object.entries(result)) {
      if (key === 'note' || typeof value !== 'string') continue;
      const row = node('div');
      row.append(node('dt', '', key.charAt(0).toUpperCase() + key.slice(1)), node('dd', '', value));
      $('channels').append(row);
    }
  } catch (error) { $('channels').replaceChildren(node('div', 'muted', errorText(error))); }
}

$('input').addEventListener('input', renderControls);
$('form').addEventListener('submit', (event) => { void sendMessage(event); });
$('session-search').addEventListener('input', renderSessions);
$('new-session').addEventListener('click', () => { void newSession(); });
for (const tab of ['conversation', 'activity', 'approvals', 'studio']) {
  $(`tab-${tab}`).addEventListener('click', () => setTab(tab));
  $(`tab-${tab}`).addEventListener('keydown', (event) => {
    if (!['ArrowRight', 'ArrowLeft', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const all = ['conversation', 'activity', 'approvals', 'studio'];
    const index = all.indexOf(tab);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? all.length - 1 : (index + (event.key === 'ArrowRight' ? 1 : all.length - 1)) % all.length;
    setTab(all[next]);
    $(`tab-${all[next]}`).focus();
  });
}
$('attachment-input').addEventListener('click', (event) => {
  if (state.sending || state.status?.busy) { event.preventDefault(); toast('Wait for the current turn to finish.'); }
});
$('attachment-input').addEventListener('change', (event) => {
  void addFiles(event.target.files || []);
  event.target.value = '';
});
const dropzone = $('composer-dropzone');
dropzone.addEventListener('dragover', (event) => { event.preventDefault(); dropzone.classList.add('dragging'); });
dropzone.addEventListener('dragleave', () => dropzone.classList.remove('dragging'));
dropzone.addEventListener('drop', (event) => {
  event.preventDefault();
  dropzone.classList.remove('dragging');
  void addFiles(event.dataTransfer?.files || []);
});
dropzone.addEventListener('paste', (event) => {
  const files = [...(event.clipboardData?.items || [])]
    .filter((item) => item.kind === 'file' && rasterMediaType(item.type))
    .map((item) => item.getAsFile())
    .filter(Boolean);
  if (files.length) { event.preventDefault(); void addFiles(files); }
});
$('studio-refresh').addEventListener('click', () => { state.studioLoaded = false; void loadStudio(); });
$('theme-load').addEventListener('click', () => { void loadTheme(); });
$('studio-provider-form').addEventListener('submit', (event) => { void saveProviderProfile(event); });
$('studio-profile-select').addEventListener('change', (event) => {
  const profile = state.studio?.provider?.profiles?.find((item) => item.id === event.target.value);
  if (!profile) return;
  fillProviderForm(state.studio.provider, profile);
  ensureModelOption(profile.models?.[0]);
});
$('studio-mcp-form').addEventListener('submit', (event) => { void addMcpServer(event); });
$('studio-skill-form').addEventListener('submit', (event) => { void installSkill(event); });
$('theme-file').addEventListener('change', async (event) => {
  const file = event.target.files?.[0];
  event.target.value = '';
  if (!file) return;
  try { $('theme-input').value = await file.text(); setError($('theme-error')); }
  catch (error) { setError($('theme-error'), errorText(error)); }
});
$('theme-apply').addEventListener('click', () => {
  try { readThemeInput(); } catch (error) { setError($('theme-error'), errorText(error)); }
});
$('theme-save').addEventListener('click', () => { void saveTheme(); });
$('theme-export').addEventListener('click', exportTheme);
$('sessions-toggle').addEventListener('click', () => toggleDrawer('sessions'));
$('inspector-toggle').addEventListener('click', () => toggleDrawer('inspector'));
$('sessions-close').addEventListener('click', closeDrawers);
$('inspector-close').addEventListener('click', () => {
  if (matchMedia('(max-width: 1170px)').matches) closeDrawers();
  else toggleDrawer('inspector');
});
$('drawer-backdrop').addEventListener('click', closeDrawers);
document.addEventListener('keydown', (event) => { if (event.key === 'Escape') closeDrawers(); });
$('event-filter').addEventListener('change', renderEvents);
$('clear-events').addEventListener('click', () => { state.events = []; $('activity-count').textContent = '0'; renderEvents(); });
$('check-health').addEventListener('click', () => { void healthChecks(); });
$('connection-open').addEventListener('click', () => { $('connection-dialog').showModal(); });
$('connection-close').addEventListener('click', () => $('connection-dialog').close());
$('connection-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  state.token = $('gateway-token').value.trim();
  invalidateAttachments();
  setError($('connection-error'));
  state.modelsLoaded = false;
  try {
    await connectGateway();
    await refresh({ transcript: true });
    await loadModels();
    $('connection-dialog').close();
    setError($('page-error'));
    void channels();
    void loadTheme();
    state.studioLoaded = false;
    toast('Connected');
  } catch (error) { setError($('connection-error'), errorText(error)); }
});
$('clear-token').addEventListener('click', () => { state.token = ''; invalidateAttachments(); $('gateway-token').value = ''; disconnectGateway(); });
$('gateway-connect').addEventListener('click', async () => {
  if (connected()) { disconnectGateway(); renderApprovals(); return; }
  try { await connectGateway(); renderApprovals(); await refresh(); }
  catch (error) { toast(errorText(error)); $('connection-dialog').showModal(); }
});
$('gateway-ping').addEventListener('click', async () => {
  const started = performance.now();
  try {
    const answer = await rpc('ping');
    $('gateway-latency').textContent = `${answer} / ${Math.round(performance.now() - started)} ms`;
    toast('Gateway responded');
  } catch (error) { toast(errorText(error)); }
});
$('gateway-status').addEventListener('click', () => { void refresh(); });
$('model').addEventListener('change', async () => {
  const oldModel = state.status?.model;
  $('model').disabled = true;
  try { assertOk(await rpc('model.set', { id: $('model').value })); await refresh(); toast('Model updated'); }
  catch (error) { if (oldModel) $('model').value = oldModel; toast(errorText(error)); }
  finally { renderControls(); }
});
$('pause-session').addEventListener('click', async () => {
  const id = state.selectedSessionId;
  const pause = !state.paused.get(id);
  $('pause-session').disabled = true;
  try {
    const result = assertOk(await rpc(pause ? 'session.pause' : 'session.resume', { id }));
    state.paused.set(id, result.paused === true);
    toast(result.paused ? 'Session paused at the next boundary' : 'Session resumed');
    renderControls();
  } catch (error) { toast(errorText(error)); renderControls(); }
});
$('context-open').addEventListener('click', () => {
  const textMessages = state.messages.filter((m) => ['user', 'assistant', 'system'].includes(m.role)).map(({ role, content }) => ({ role, content }));
  $('context-input').value = JSON.stringify(textMessages, null, 2);
  setError($('context-error'));
  $('context-dialog').showModal();
});
for (const id of ['context-close', 'context-cancel']) $(id).addEventListener('click', () => $('context-dialog').close());
$('context-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  $('context-save').disabled = true;
  setError($('context-error'));
  try {
    const messages = parseContext($('context-input').value);
    assertOk(await rpc('session.context', { id: state.selectedSessionId, messages }));
    $('context-dialog').close();
    toast('Context queued for the next turn');
  } catch (error) { setError($('context-error'), errorText(error)); }
  finally { $('context-save').disabled = false; }
});

async function start() {
  updateConnection('connecting');
  await refresh({ transcript: true });
  void loadModels().catch((error) => toast(errorText(error)));
  void channels();
  void loadTheme();
  try { await connectGateway(); renderApprovals(); }
  catch { updateConnection('offline'); }
  renderControls();
}
void start();
setInterval(() => {
  approvals.expire();
  renderApprovals();
  if (document.visibilityState === 'visible') void refresh();
}, 5000);
