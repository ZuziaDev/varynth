const HUNK = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/;

export function parseUnifiedDiff(text) {
  const files = [];
  let file;
  let oldLine = 0;
  let newLine = 0;
  let insideHunk = false;
  let deletions = [];
  let additions = [];
  const createFile = (name = 'Changes') => {
    file = { oldName: name, newName: name, rows: [], added: 0, removed: 0 };
    files.push(file);
    insideHunk = false;
  };
  const ensureFile = () => { if (!file) createFile(); };
  const flush = () => {
    if (!file) return;
    const count = Math.max(deletions.length, additions.length);
    for (let i = 0; i < count; i++) {
      file.rows.push({ kind: 'change', old: deletions[i] || null, new: additions[i] || null });
    }
    deletions = [];
    additions = [];
  };
  for (const line of String(text).split(/\r?\n/)) {
    if (line.startsWith('diff --git ')) {
      flush();
      createFile(line.slice(11));
    } else if (!insideHunk && line.startsWith('--- ')) {
      flush();
      if (file?.rows.length) createFile();
      ensureFile();
      file.oldName = line.slice(4).split('\t')[0];
    } else if (!insideHunk && line.startsWith('+++ ')) {
      ensureFile();
      file.newName = line.slice(4).split('\t')[0];
    } else if (HUNK.test(line)) {
      flush(); ensureFile();
      const match = line.match(HUNK);
      oldLine = Number(match[1]);
      newLine = Number(match[3]);
      file.rows.push({ kind: 'hunk', text: line });
      insideHunk = true;
    } else if (insideHunk && line.startsWith('-')) {
      deletions.push({ line: oldLine++, text: line.slice(1), kind: 'removed' });
      file.removed++;
    } else if (insideHunk && line.startsWith('+')) {
      additions.push({ line: newLine++, text: line.slice(1), kind: 'added' });
      file.added++;
    } else if (insideHunk && line.startsWith(' ')) {
      flush();
      file.rows.push({ kind: 'context', old: { line: oldLine++, text: line.slice(1) }, new: { line: newLine++, text: line.slice(1) } });
    } else if (line.startsWith('\\')) {
      flush(); ensureFile();
      file.rows.push({ kind: 'note', text: line });
    } else if (/^(Binary files|rename |new file|deleted file|old mode|new mode|similarity)/.test(line)) {
      flush(); ensureFile();
      file.rows.push({ kind: 'note', text: line });
    }
  }
  flush();
  return files.filter((item) => item.rows.length || item.oldName !== 'Changes' || item.newName !== 'Changes');
}

export function sideBySideRows(text) {
  return parseUnifiedDiff(text).flatMap((file) => file.rows);
}

function element(tag, className, text) {
  const result = document.createElement(tag);
  if (className) result.className = className;
  if (text !== undefined) result.textContent = text;
  return result;
}

function renderDiff(text) {
  const output = document.getElementById('out');
  const files = parseUnifiedDiff(text);
  output.replaceChildren();
  if (!files.length) {
    output.append(element('div', 'quiet-empty', text.trim() ? 'No unified diff hunks found.' : 'No diff loaded.'));
    document.getElementById('diff-summary').textContent = 'No changes';
    return;
  }
  let added = 0;
  let removed = 0;
  for (const file of files) {
    added += file.added; removed += file.removed;
    const section = element('section', 'diff-file');
    const heading = element('div', 'diff-file-heading');
    heading.append(element('h2', '', file.newName === '/dev/null' ? file.oldName : file.newName));
    const stats = element('span', 'diff-stats');
    stats.append(element('span', 'added', `+${file.added}`), element('span', 'removed', `−${file.removed}`));
    heading.append(stats);
    const table = element('table', 'diff-table');
    table.setAttribute('aria-label', `Changes in ${file.newName}`);
    const thead = element('thead');
    const labels = element('tr');
    const old = element('th', '', 'Before'); old.colSpan = 2;
    const next = element('th', '', 'After'); next.colSpan = 2;
    labels.append(old, next); thead.append(labels);
    const tbody = element('tbody');
    for (const row of file.rows) {
      const tr = element('tr', row.kind);
      if (row.kind === 'hunk' || row.kind === 'note') {
        const cell = element('td', 'diff-note', row.text); cell.colSpan = 4; tr.append(cell);
      } else {
        for (const [side, cell] of [['before', row.old], ['after', row.new]]) {
          const kind = cell?.kind || 'context';
          const number = element('td', `diff-line ${side} ${kind}`, cell?.line == null ? '' : String(cell.line));
          const source = element('td', `diff-source ${side} ${kind}`, cell?.text || '');
          source.dataset.line = cell?.line == null ? '' : String(cell.line);
          tr.append(number, source);
        }
      }
      tbody.append(tr);
    }
    table.append(thead, tbody);
    section.append(heading, table);
    output.append(section);
  }
  document.getElementById('diff-summary').textContent = `${files.length} ${files.length === 1 ? 'file' : 'files'} / +${added} −${removed}`;
}

if (typeof document !== 'undefined') {
  const input = document.getElementById('diff-input');
  document.getElementById('diff-form').addEventListener('submit', (event) => { event.preventDefault(); renderDiff(input.value); });
  document.getElementById('diff-example').addEventListener('click', () => {
    input.value = 'diff --git a/src/config.rs b/src/config.rs\n--- a/src/config.rs\n+++ b/src/config.rs\n@@ -1,4 +1,5 @@\n pub fn validate(&self) -> Result<()> {\n-    Ok(())\n+    self.validate_model()?;\n+    self.validate_permissions()?;\n+    Ok(())\n }';
    renderDiff(input.value);
  });
  document.getElementById('diff-clear').addEventListener('click', () => { input.value = ''; renderDiff(''); });
  const passed = new URLSearchParams(location.search).get('diff');
  if (passed) { input.value = passed; renderDiff(passed); }
}
