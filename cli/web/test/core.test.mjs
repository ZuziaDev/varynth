import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ApprovalLedger, normalizeApproval, parseContext, selectedAfterRefresh,
  SseDecoder, messageParts, eventMatchesFilter, matchesSessionQuery,
  autocompactLabel, approvalFromPending, isRelayId, formatUptime, percentOf,
  channelRows, composerKeyAction,
} from '../assets/core.mjs';
import { sideBySideRows, parseUnifiedDiff } from '../assets/diff.mjs';

test('channel rows map server status strings to tones', () => {
  const { rows, note } = channelRows({
    telegram: 'token set, pairing (empty allowlist)',
    discord: 'planned',
    dashboard: 'live',
    whatsapp: 'off — set VARYNTH_TELEGRAM_BOT_TOKEN',
    extra: 42,
    note: 'Polls while serve runs.',
  });
  assert.equal(note, 'Polls while serve runs.');
  assert.deepEqual(rows.map((row) => [row.name, row.tone, row.label]), [
    ['Telegram', 'waiting', 'Pairing'],
    ['Discord', 'planned', 'Planned'],
    ['Dashboard', 'good', 'Live'],
    ['Whatsapp', 'off', 'Off'],
  ]);
  assert.equal(rows[2].detail, '');
  assert.match(rows[3].detail, /VARYNTH_TELEGRAM_BOT_TOKEN/);
  assert.deepEqual(channelRows(null), { rows: [], note: '' });
  assert.deepEqual(channelRows([1]), { rows: [], note: '' });
});

test('composer key action sends on Enter but respects Shift and IME', () => {
  assert.equal(composerKeyAction({ key: 'Enter' }), 'send');
  assert.equal(composerKeyAction({ key: 'Enter', ctrlKey: true }), 'send');
  assert.equal(composerKeyAction({ key: 'Enter', shiftKey: true }), 'none');
  assert.equal(composerKeyAction({ key: 'Enter', isComposing: true }), 'none');
  assert.equal(composerKeyAction({ key: 'Enter', keyCode: 229 }), 'none');
  assert.equal(composerKeyAction({ key: 'a' }), 'none');
  assert.equal(composerKeyAction(null), 'none');
  // Touch: plain Enter inserts a newline; Ctrl/Cmd+Enter still sends.
  assert.equal(composerKeyAction({ key: 'Enter' }, { touch: true }), 'none');
  assert.equal(composerKeyAction({ key: 'Enter', metaKey: true }, { touch: true }), 'send');
});

const ID = '12345678-1234-1234-1234-123456789012';

test('SSE decoder handles split frames and preserves deltas', () => {
  const decoder = new SseDecoder();
  assert.deepEqual(decoder.feed('event: delta\ndata: {"kind":"delta","text":"hel'), []);
  assert.deepEqual(decoder.feed('lo"}\n\n'), [{ kind: 'delta', text: 'hello' }]);
});

test('autocompact label shows the percent line or off', () => {
  assert.equal(autocompactLabel(null), '—');
  assert.equal(autocompactLabel({ enabled: false, percent: 76, threshold_tokens: 72960 }), 'off');
  assert.equal(autocompactLabel({ enabled: true, percent: 76, threshold_tokens: 72960 }), '76% · 72,960');
  assert.equal(autocompactLabel({ enabled: true, percent: 80 }), '80%');
});

test('SSE handles CRLF boundaries and comment keepalives', () => {
  const decoder = new SseDecoder();
  assert.deepEqual(decoder.feed(': keepalive\r\n\r'), []);
  assert.deepEqual(decoder.feed('\nevent: done\r\ndata: {"text":"ok"}\r\n\r\n'), [{ kind: 'done', text: 'ok' }]);
  decoder.finish();
});

test('SSE rejects malformed JSON and incomplete final frames', () => {
  assert.throws(() => new SseDecoder().feed('data: {bad}\n\n'));
  const decoder = new SseDecoder();
  decoder.feed('data: {"kind":"done"}');
  assert.throws(() => decoder.finish());
});

test('approval ledger deduplicates and resolves one-shot requests', () => {
  const ledger = new ApprovalLedger();
  const request = normalizeApproval({ kind: 'approval_pending', text: JSON.stringify({ id: ID, tool: 'bash', detail: 'git status' }) }, 'session');
  assert.equal(ledger.add(request), true);
  assert.equal(ledger.add(request), false);
  ledger.resolve(request.id);
  assert.equal(ledger.pending.size, 0);
  assert.equal(ledger.add(request), false);
});

test('raw id-less notices cannot create actionable approvals', () => {
  assert.equal(normalizeApproval({ kind: 'approval_request', text: 'write_file example.txt' }, 's'), null);
  assert.equal(normalizeApproval({ kind: 'approval_pending', text: '{}' }, 's'), null);
  assert.equal(normalizeApproval({ kind: 'approval_request', id: 'wrong', text: 'request' }, 's'), null);
});

test('structured and pending JSON approvals share the same relay id', () => {
  const ledger = new ApprovalLedger();
  const pending = normalizeApproval({ kind: 'approval_pending', text: JSON.stringify({ id: ID, tool: 'write_file', detail: 'a.txt' }) }, 's', 100);
  const structured = normalizeApproval({ kind: 'approval_request', id: ID, tool: 'write_file', detail: 'a.txt' }, 's', 200);
  ledger.add(pending);
  ledger.add(structured);
  assert.equal(ledger.pending.size, 1);
  assert.equal(ledger.pending.get(ID).createdAt, 100);
});

test('expired approvals stay retired when delayed frames arrive', () => {
  const ledger = new ApprovalLedger();
  const request = normalizeApproval({ kind: 'approval_request', id: ID, tool: 'bash', detail: 'test' }, 's', 0);
  ledger.add(request);
  ledger.expire(126000);
  assert.equal(ledger.pending.size, 0);
  assert.equal(ledger.add(request), false);
});

test('context parser rejects tool content and image payloads', () => {
  assert.deepEqual(parseContext('[{"role":"user","content":"hello"}]'), [{ role: 'user', content: 'hello' }]);
  assert.throws(() => parseContext('[{"role":"tool","content":"no"}]'));
  assert.throws(() => parseContext('[{"role":"assistant","content":"x","tool_calls":[{}]}]'));
  assert.throws(() => parseContext('[{"role":"user","content":"x","images":[{}]}]'));
});

test('context parser enforces message and byte bounds', () => {
  assert.throws(() => parseContext(JSON.stringify(Array.from({length: 201}, () => ({role: 'user', content: 'x'})))));
  assert.throws(() => parseContext(JSON.stringify([{role: 'user', content: 'x'.repeat(512 * 1024)}])));
});

test('session selection is stable across refresh', () => {
  assert.equal(selectedAfterRefresh('history-id', 'active-id'), 'history-id');
  assert.equal(selectedAfterRefresh(null, 'active-id'), 'active-id');
});

test('session search works with title, model and id', () => {
  const session = {id: 'abc123', title: 'Authentication review', model: 'local-fast'};
  assert(matchesSessionQuery(session, 'authentication'));
  assert(matchesSessionQuery(session, 'LOCAL'));
  assert(matchesSessionQuery(session, 'abc'));
  assert(!matchesSessionQuery(session, 'unrelated'));
});

test('pending approvals normalize into the live ledger shape', () => {
  assert(isRelayId(ID));
  assert(!isRelayId('../../escape'));
  const request = approvalFromPending({ id: ID, tool: 'bash', detail: 'git status' }, 1234);
  assert.deepEqual(request, {
    id: ID, tool: 'bash', detail: 'git status', session: '',
    createdAt: 1234, sending: false, error: '',
  });
  assert.equal(approvalFromPending({ id: 'nope' }), null);
  const ledger = new ApprovalLedger();
  assert.equal(ledger.add(request), true);
  assert.equal(ledger.add(approvalFromPending({ id: ID, tool: 'bash' })), false);
  assert.equal(ledger.pending.size, 1);
});

test('uptime and percent formatting stay bounded', () => {
  assert.equal(formatUptime(0), '0s');
  assert.equal(formatUptime(59), '59s');
  assert.equal(formatUptime(64), '1m 04s');
  assert.equal(formatUptime(3_600 + 7 * 60), '1h 07m');
  assert.equal(formatUptime(86_400 * 3 + 4 * 3_600), '3d 04h');
  assert.equal(formatUptime(-1), '—');
  assert.equal(formatUptime(Number.NaN), '—');
  assert.equal(percentOf(25, 100), 25);
  assert.equal(percentOf(500, 100), 100);
  assert.equal(percentOf(-2, 100), null);
  assert.equal(percentOf(5, 0), null);
  assert.equal(percentOf(5, Number.NaN), null);
});

test('code fences preserve untrusted text without interpreting markup', () => {
  const parts = messageParts('Answer\n```html\n<script>bad()</script>\n```\nEnd');
  assert.equal(parts[1].kind, 'code');
  assert.equal(parts[1].text, '<script>bad()</script>');
  assert.equal(parts[2].text.trim(), 'End');
});

test('activity filters distinguish tool, error and system events', () => {
  assert(eventMatchesFilter('tool_result', 'tools'));
  assert(!eventMatchesFilter('assistant', 'tools'));
  assert(eventMatchesFilter('error', 'errors'));
  assert(eventMatchesFilter('round', 'system'));
});

test('diff rows align additions and removals', () => {
  const rows = sideBySideRows('@@ -1,2 +1,2 @@\n-old\n+new\n same');
  assert.equal(rows[1].old.text, 'old');
  assert.equal(rows[1].new.text, 'new');
  assert.equal(rows[2].kind, 'context');
});

test('diff parser separates files and preserves line numbers', () => {
  const files = parseUnifiedDiff('diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -4 +4 @@\n-before\n+after\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1,2 @@\n old\n+added');
  assert.equal(files.length, 2);
  assert.equal(files[0].removed, 1);
  assert.equal(files[0].rows[1].old.line, 4);
  assert.equal(files[1].newName, 'b/b.rs');
  assert.equal(files[1].added, 1);
});
