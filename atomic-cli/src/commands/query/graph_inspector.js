// Embedded into exported HTML. This inspector reads only the exported snapshot;
// it never fetches from a service or treats graph-controlled strings as HTML.
function createGraphInspector(container, graphData, onSelect) {
  var doc = container.ownerDocument;
  var byId = new Map(graphData.nodes.map(function(node) { return [node.id, node]; }));
  var incoming = new Map();
  var outgoing = new Map();
  function endpoint(value) { return typeof value === 'object' && value ? value.id : value; }
  function compact(value) { return String(value).replace('https://atomic.dev/ns/workflow#', 'atomic:'); }
  function element(tag, text, className) {
    var el = doc.createElement(tag);
    if (text !== undefined) el.textContent = text;
    if (className) el.className = className;
    return el;
  }
  graphData.links.forEach(function(edge) {
    var source = endpoint(edge.source);
    var target = endpoint(edge.target);
    if (!outgoing.has(source)) outgoing.set(source, []);
    if (!incoming.has(target)) incoming.set(target, []);
    outgoing.get(source).push({ edge: edge, other: target });
    incoming.get(target).push({ edge: edge, other: source });
  });

  var heading = element('div', undefined, 'inspector-heading');
  heading.appendChild(element('h2', 'Node details'));
  var clear = element('button', 'Clear', 'inspector-clear');
  clear.type = 'button';
  clear.setAttribute('aria-label', 'Clear selected node');
  heading.appendChild(clear);
  var label = element('label', 'Select a node', 'inspector-picker-label');
  label.htmlFor = 'node-picker';
  var picker = element('select', undefined, 'inspector-picker');
  picker.id = 'node-picker';
  var placeholder = element('option', 'Click a graph node or choose here');
  placeholder.value = '';
  picker.appendChild(placeholder);
  graphData.nodes.forEach(function(node) {
    var text = node.label || node.id;
    var option = element('option', text.length > 100 ? text.slice(0, 97) + '...' : text);
    option.value = node.id;
    picker.appendChild(option);
  });
  var status = element('p', '', 'inspector-status');
  status.setAttribute('role', 'status');
  var content = element('div', undefined, 'inspector-content');
  container.replaceChildren(heading, label, picker, status, content);

  function section(title) {
    var result = element('section', undefined, 'inspector-section');
    result.appendChild(element('h3', title));
    content.appendChild(result);
    return result;
  }
  function field(title, text) {
    section(title).appendChild(element('pre', text, 'inspector-value'));
  }
  function tokenTable(parent, tokens) {
    var table = element('table', undefined, 'inspector-token-table');
    [['Input', 'input'], ['Output', 'output'], ['Reasoning', 'reasoning'],
      ['Cache read', 'cache_read'], ['Cache write', 'cache_write'], ['Recorded total', 'total']].forEach(function(pair) {
      var row = element('tr');
      row.appendChild(element('th', pair[0]));
      row.appendChild(element('td', tokens ? Number(tokens[pair[1]]).toLocaleString('en-US') : 'Not recorded'));
      table.appendChild(row);
    });
    parent.appendChild(table);
  }
  function usageDetails(node) {
    var usage = node.metadata && node.metadata.ai_usage;
    var types = node.types || [node.kind];
    var relevant = types.some(function(type) {
      return /#(Change|View|WorkIntent|FeatureIntent|BugIntent|ReviewIntent|ManagedIntent)$/.test(type);
    });
    if (!usage) {
      if (relevant) section('AI usage').appendChild(element('p', 'Usage was not included in this export. Regenerate it with a current Atomic CLI.', 'inspector-muted'));
      return;
    }
    var result = section('AI usage');
    result.classList.add('inspector-ai-usage');
    var scope = {
      change_provenance: 'This change’s recorded provenance.',
      intent_implementation_and_recording_evidence_excluding_dependencies: 'Intent implementation and intent-artifact recording evidence; dependency-only changes excluded.',
      view_effective_membership_including_ancestors_and_dependencies: 'Effective view membership, including ancestor views and dependencies.',
      view_canonical_effective_membership_including_ancestors_and_dependencies: 'Effective view membership from canonical repository data, including ancestor views and dependencies.'
    };
    result.appendChild(element('p', scope[usage.basis] || usage.basis, 'inspector-muted'));
    var coverage = usage.coverage;
    result.appendChild(element('p', coverage.with_provenance + ' of ' + coverage.changes + ' unique changes have AI provenance; ' + coverage.without_provenance + ' without provenance, ' + coverage.unavailable + ' unavailable.', 'inspector-usage-coverage'));
    if (!usage.scope_complete) result.appendChild(element('p', 'Incomplete membership scope: ' + (usage.scope_error || 'some ancestor views are unavailable') + '. These totals are not complete.', 'inspector-usage-warning'));
    if (coverage.duplicate_usage_records) result.appendChild(element('p', coverage.duplicate_usage_records + ' duplicate usage record(s) counted once.', 'inspector-muted'));
    if (coverage.conflicting_usage_records) result.appendChild(element('p', coverage.conflicting_usage_records + ' conflicting usage record(s) excluded from totals.', 'inspector-usage-warning'));
    if (coverage.records_without_cost) result.appendChild(element('p', coverage.records_without_cost + ' usage record(s) have no usable cost. Amount below is a known subtotal.', 'inspector-usage-warning'));
    if (coverage.records_without_tokens) result.appendChild(element('p', coverage.records_without_tokens + ' usage record(s) have no reported token counts. Token values below are known subtotals.', 'inspector-usage-warning'));
    result.appendChild(element('div', usage.cost ? '$' + usage.cost.amount + ' ' + usage.cost.currency : 'Cost not recorded', 'inspector-usage-cost'));
    tokenTable(result, usage.tokens);
    result.appendChild(element('p', 'Cache and reasoning categories are provider-defined: they may overlap or be excluded from the recorded total.', 'inspector-muted'));
    if (usage.contributors && usage.contributors.length) {
      var attribution = section('Models, tools & vendors');
      usage.contributors.forEach(function(profile) {
        var card = element('div', undefined, 'inspector-attribution');
        card.appendChild(element('h4', profile.model || '(model not recorded)'));
        card.appendChild(element('p', 'Vendor: ' + (profile.vendor || '(not recorded)')));
        card.appendChild(element('p', 'Tool: ' + (profile.tool || '(not recorded)')));
        card.appendChild(element('p', profile.cost_usd === null ? 'Cost not recorded' : '$' + profile.cost_usd + ' USD'));
        var breakdown = element('details');
        breakdown.appendChild(element('summary', 'Token breakdown for this model/tool'));
        tokenTable(breakdown, profile.tokens);
        card.appendChild(breakdown);
        attribution.appendChild(card);
      });
    }
    var notes = element('details', undefined, 'inspector-usage-notes');
    notes.appendChild(element('summary', 'How these totals are calculated'));
    (usage.notes || []).forEach(function(note) { notes.appendChild(element('p', note, 'inspector-muted')); });
    result.appendChild(notes);
  }
  function relationships(title, entries) {
    var list = section(title + ' (' + entries.length + ')');
    if (!entries.length) list.appendChild(element('p', 'None in this exported graph.', 'inspector-muted'));
    entries.forEach(function(entry) {
      var row = element('div', undefined, 'inspector-relation');
      row.appendChild(element('div', compact(entry.edge.kind), 'inspector-predicate'));
      var neighbor = byId.get(entry.other);
      if (neighbor) {
        var button = element('button', neighbor.label || neighbor.id, 'inspector-neighbor');
        button.type = 'button';
        button.title = entry.other;
        button.addEventListener('click', function() { select(entry.other); picker.focus(); });
        row.appendChild(button);
      }
      row.appendChild(element('code', entry.other, 'inspector-node-id'));
      if (entry.edge.metadata !== undefined && entry.edge.metadata !== null) {
        var details = element('details');
        details.appendChild(element('summary', 'Relationship metadata'));
        details.appendChild(element('pre', JSON.stringify(entry.edge.metadata, null, 2), 'inspector-value'));
        row.appendChild(details);
      }
      list.appendChild(row);
    });
  }
  function select(id) {
    var node = byId.get(id);
    picker.value = node ? node.id : '';
    clear.disabled = !node;
    container.dataset.selectedNode = node ? node.id : '';
    content.replaceChildren();
    container.scrollTop = 0;
    if (!node) {
      status.textContent = 'No node selected.';
      content.appendChild(element('p', 'Click a node to pin its details here. Follow a relationship to inspect its other endpoint. Escape or Clear resets selection.', 'inspector-muted'));
    } else {
      status.textContent = 'Selected: ' + (node.label || node.id);
      field('Label', node.label || '(no label)');
      field('Canonical ID', node.id);
      field('Types', (node.types || [node.kind]).join('\n'));
      field('Source', node.source || '(not recorded)');
      field('Summary', node.summary || '(no summary recorded)');
      usageDetails(node);
      var metadata = section('Metadata');
      if (node.metadata === undefined || node.metadata === null) {
        metadata.appendChild(element('p', 'No metadata exported.', 'inspector-muted'));
      } else if (typeof node.metadata === 'object' && !Array.isArray(node.metadata)) {
        Object.keys(node.metadata).filter(function(key) {
          return !['ai_usage', 'ai_usage_records', 'ai_usage_state', 'ai_usage_record_version'].includes(key);
        }).sort().forEach(function(key) {
          metadata.appendChild(element('h4', key));
          var value = node.metadata[key];
          metadata.appendChild(element('pre', typeof value === 'string' ? value : JSON.stringify(value, null, 2), 'inspector-value'));
        });
      } else {
        metadata.appendChild(element('pre', JSON.stringify(node.metadata, null, 2), 'inspector-value'));
      }
      relationships('Outgoing relationships', outgoing.get(node.id) || []);
      relationships('Incoming relationships', incoming.get(node.id) || []);
      var raw = element('details', undefined, 'inspector-section');
      raw.appendChild(element('summary', 'Raw exported node'));
      raw.appendChild(element('pre', JSON.stringify(node, null, 2), 'inspector-value'));
      content.appendChild(raw);
      content.appendChild(element('p', 'Snapshot data only. File contents and historical patch bodies are not fetched.', 'inspector-muted'));
    }
    if (onSelect) onSelect(node ? node.id : null);
  }
  picker.addEventListener('change', function() { select(picker.value); });
  clear.addEventListener('click', function() { select(null); picker.focus(); });
  doc.addEventListener('keydown', function(event) {
    if (event.key === 'Escape') { select(null); picker.focus(); }
  });
  // Build the initial empty state without calling the graph's redraw callback
  // before its canvas/simulation initialization has finished.
  var callback = onSelect;
  onSelect = null;
  select(null);
  onSelect = callback;
  return { select: select };
}
