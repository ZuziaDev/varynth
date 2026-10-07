import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ApprovalLedger, normalizeApproval, parseContext, selectedAfterRefresh,
  SseDecoder, messageParts, eventMatchesFilter, matchesSessionQuery,
} from '../assets/core.mjs';
import { sideBySideRows, parseUnifiedDiff } from '../assets/diff.mjs';

const ID = '12345678-1234-1234-1234-123456789012';

test('SSE decoder handles split frames and preserves deltas', () => {
  const decoder = new SseDecoder();
  assert.deepEqual(decoder.feed('event: delta\ndata: {"kind":"delta","text":"hel'), []);
  assert.deepEqual(decoder.feed('lo"}\n\n'), [{ kind: 'delta', text: 'hello' }]);
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
