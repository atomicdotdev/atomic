(() => {
  'use strict';
  const data = JSON.parse(document.getElementById('review-data').textContent);
  const report = data.view;
  const graph = report.graph;
  const viewSnapshot = report.view_snapshot;
  const viewComparison = report.view_comparison;
  const viewConformance = viewComparison?.conformance || viewSnapshot?.conformance;
  const comparedFiles = new Map((viewComparison?.files || []).map(file=>[file.source_ref,file]));
  const regions = data.regions;
  const byId = new Map(graph.nodes.map(node => [node.id, node]));
  const findings = new Map(graph.findings.map(finding => [finding.number, finding]));
  const snapshots = new Map(data.sources.map(source => [source.change + '\0' + source.path, source]));
  const passes = new Map(graph.passes.map(pass => [pass.pass, pass]));
  const metricId = node => 'metric.' + node.properties.observation.metric.replaceAll('_', '-');
  const metricCards = new Map(data.metric_cards.map(card => ['metric.' + card.metric.replaceAll('_', '-'), card]));
  const auditCards = new Map(data.audit_cards.map(card => [card.id, card]));
  const risk = graph.risk;
  const riskRules = new Map((risk?.model?.rules || []).map(rule => [rule.id, rule]));
  const sections = ['summary', 'slop', 'conformance', 'risk'];
  const rememberedRegions = { slop: null, risk: null };
  const state = { pass: 'summary', region: null, intent: null, criterion: null, criterionRegion: null, query: '', filter: viewSnapshot ? 'all' : 'concerns' };
  const criterionFindings=new Map();
  for(const finding of graph.findings)for(const id of finding.related_criteria) {
    if(!criterionFindings.has(id))criterionFindings.set(id,new Set());
    criterionFindings.get(id).add(finding.number);
  }
  const intentCatalog = new Map((viewConformance?.intents || []).map(intent=>[intent.id,{...intent,scoped:true,criteria:[]}]));
  for(const criterion of graph.criteria) {
    if(!intentCatalog.has(criterion.intent)) {
      const linked=(viewConformance?.changes || []).flatMap(change=>change.intent_context.plans || []).map(plan=>plan.intent).find(intent=>intent && (criterion.intent===intent.id || criterion.intent.endsWith(':'+intent.uid)));
      intentCatalog.set(criterion.intent,{id:criterion.intent,human_key:linked?.id || criterion.intent,title:linked?.title || 'Change-linked intent',status:'',view:null,scoped:false,criteria:[]});
    }
    intentCatalog.get(criterion.intent).criteria.push({...criterion,navigation_findings:[...new Set([...criterion.findings,...(criterionFindings.get(criterion.id) || [])])]});
  }
  const $ = id => document.getElementById(id);
  const el = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  };
  const button = (text, action, className = '') => {
    const node = el('button', className, text);
    node.type = 'button';
    node.addEventListener('click', action);
    return node;
  };
  const title = text => String(text || '').replaceAll('_', ' ').replace(/^./, c => c.toUpperCase());
  const shown = value => value === null || value === undefined ? 'unavailable' : String(value);
  const badge = (text, value = text) => el('span', 'badge ' + String(value).toLowerCase().replace(/[^a-z]/g, ''), text);
  const detail = (label, content) => {
    const node = el('details', 'coverage');
    node.append(el('summary', '', label), content);
    return node;
  };
  const definition = (entries) => {
    const list = el('dl');
    for (const [name, value] of entries) list.append(el('dt', '', name), el('dd', '', value));
    return list;
  };
  const cardCallout = (id, text) => button(text || id, () => showCard(id, true), 'card-callout');
  const location = region => region.start_line ? `Lines ${region.start_line}–${region.end_line} · ${viewSnapshot ? 'snapshot' : region.side}` : 'File-level observation · range unavailable';
  const regionName = region => {
    const subjects = region.subjects.filter(subject => subject !== region.path);
    return subjects.length > 1 ? subjects.length + ' source subjects' : subjects[0] || 'File-level facts';
  };
  const inPass = (finding, pass) => finding.passes.includes(pass === 'slop' ? 'anti_slop' : pass === 'risk' ? 'blast_radius_risk' : 'intent_conformance');
  const regionFindings = region => region.findings.map(number => findings.get(number)).filter(Boolean);
  const numberText = region => region.findings.map(number => 'F' + number).join(', ');
  const changes = new Map(report.changes.map(change => [change.hash, change]));

  function describeValue(value) {
    if (value.status === 'measured') return shown(value.value);
    return value.status === 'not_applicable' ? 'N/A' : 'Unavailable';
  }
  function isAbsent(value) {
    return value.status === 'not_applicable' && ['File is absent in this revision', 'Subject is not present in this revision'].includes(value.reason);
  }
  function changeText(node) {
    if (viewSnapshot) return 'Snapshot value; no comparison';
    const observation = node.properties.observation;
    if (typeof node.properties.delta === 'number') return (node.properties.delta > 0 ? '+' : '') + node.properties.delta;
    if (isAbsent(observation.before) && observation.after.status === 'measured') return 'New subject';
    if (isAbsent(observation.after) && observation.before.status === 'measured') return 'Removed subject';
    return 'Not comparable';
  }
  function changed(node) {
    if (!node) return false;
    const value = node.properties.observation;
    return typeof node.properties.delta === 'number' ? node.properties.delta !== 0 :
      (isAbsent(value.before) && value.after.status === 'measured') || (isAbsent(value.after) && value.before.status === 'measured');
  }

  function showCard(id, focus = false) {
    if (state.pass === 'summary') activate(riskRules.has(id) ? 'risk' : 'slop');
    const host = $('card-content');
    host.replaceChildren();
    if (!Array.from($('card-picker').options).some(option => option.value === id)) {
      const option = el('option', '', id);
      option.value = id;
      $('card-picker').append(option);
    }
    $('card-picker').value = id;
    const audit = auditCards.get(id), metric = metricCards.get(id), rule = riskRules.get(id);
    const heading = el('h2', '', audit?.title || metric?.title || (rule ? 'Change-risk rule' : 'Engineering rule'));
    heading.tabIndex = -1;
    host.append(el('p', 'card-id', id), heading);
    if (audit) {
      host.append(el('p', '', audit.question), definition([
        ['Supported patterns', data.patterns[id] || 'See the finding evidence'],
        ['Evidence to inspect', audit.evidence], ['Exceptions and limits', audit.exceptions]
      ]));
      const checks = graph.nodes.filter(node => node.kind === 'check' && node.properties.rule === id);
      const count = status => checks.filter(node => node.properties.status === status).length;
      host.append(el('p', 'muted', `${count('completed')} completed · ${count('skipped')} skipped · ${count('error')} errors`));
      host.append(detail('Exact check coverage', el('pre', '', checks.map(node => `${node.properties.path}: ${node.properties.status}\n${node.properties.detail}`).join('\n\n'))));
    } else if (metric) {
      host.append(definition([
        ['Definition', metric.definition], ['Scope and unit', `${metric.scope} · ${metric.unit}`],
        ['How to interpret it', metric.interpretation], ['Exceptions', metric.exceptions]
      ]));
      const nodes = graph.nodes.filter(node => node.kind === 'metric' && metricId(node) === id);
      const measured = nodes.filter(node => ['before', 'after'].some(side => node.properties.observation[side].status === 'measured')).length;
      host.append(el('p', 'muted', `${nodes.length} observations; ${measured} have a numeric value. These are measurements, not findings.`));
      const conventions = [...new Set(nodes.map(node => node.properties.convention).filter(Boolean))];
      host.append(el('p', 'muted', `Counting convention: ${conventions.join(', ') || 'not reported'}. Unavailable values are never zero baselines.`));
      const samples = nodes.map(node => {
        const item = node.properties, sample = item.observation;
        const value = side => describeValue(sample[side]) + (sample[side].reason ? ' — ' + sample[side].reason : '');
        const location = side => item[side + '_location'] ? `${item[side + '_location'].start_line}–${item[side + '_location'].end_line}` : 'no recorded range';
        return viewSnapshot ? `${viewSnapshot.view} @ ${viewSnapshot.state} · ${item.path}\n${sample.subject}\nSnapshot (${location('after')}): ${value('after')}` : `${item.change} · ${item.path}\n${sample.subject}\nBefore (${location('before')}): ${value('before')}\nAfter (${location('after')}): ${value('after')}\n${changeText(node)}`;
      });
      host.append(detail('All recorded observations', el('pre', '', samples.join('\n\n') || 'No recorded observations for this metric in the selected scope.')));
    } else if (rule) {
      host.append(badge(title(rule.level), rule.level), el('p', '', rule.reason));
      host.append(el('p', 'muted', `${risk.model.model}: Atomic review-priority policy, not defect probability or a confirmed bug.`));
      host.append(detail('Expanded threshold profile', el('pre', '', data.profile_json || 'Profile not exported')));
      host.append(detail('Recorded references', el('pre', '', rule.references.join('\n'))));
    } else {
      const finding = graph.findings.find(item => item.finding.requirement_or_rule.id === id);
      host.append(el('p', '', finding?.finding.requirement_or_rule.explanation || 'No definition was supplied for this rule.'));
    }
    if (focus) heading.focus();
  }

  function findingBlock(number) {
    const item = findings.get(number), finding = item.finding;
    const host = el('article', 'finding');
    host.dataset.finding=String(number);
    const heading = el('h3');
    heading.append(el('span', 'finding-number', 'F' + number), el('span', '', finding.summary), badge(title(finding.severity), finding.severity));
    if (item.comparison) heading.append(badge(title(item.comparison.status),item.comparison.status));
    host.append(heading);
    const links = el('div', 'callouts');
    links.append(cardCallout(finding.requirement_or_rule.id, 'Audit: ' + (auditCards.get(finding.requirement_or_rule.id)?.title || finding.requirement_or_rule.id)));
    host.append(links, el('p', 'region-meta', viewComparison ? `Confidence: ${title(finding.confidence)} · ${title(item.comparison?.status || 'unknown')} local observation between the selected views` : `Confidence: ${title(finding.confidence)} · Introduced by this change: ${finding.introduced_by_change.status}`));
    const evidence = el('div', 'evidence');
    if (item.comparison) evidence.append(el('p','',item.comparison.rationale));
    evidence.append(el('p', '', finding.introduced_by_change.rationale));
    for (const entry of finding.evidence) {
      evidence.append(el('p', 'source-path', entry.reference), el('pre', '', entry.observation));
    }
    evidence.append(el('h3', '', 'Proposed correction'), el('p', '', finding.proposed_correction));
    for (const criterion of finding.acceptance_criteria || []) evidence.append(el('p', '', `${criterion.criterion}: ${criterion.assessment} — ${criterion.rationale}`));
    if (!(finding.acceptance_criteria || []).length) evidence.append(el('p', 'muted', 'Engineering-rule concern; no unmet acceptance criterion was inferred.'));
    host.append(detail('Evidence, attribution & correction', evidence));
    return host;
  }

  function metricTable(region) {
    const host = el('section', 'metrics-block');
    const own = region.metrics.length ? region.metrics : region.context_metrics;
    if (!own.length) return host;
    host.append(el('h3', '', region.metrics.length ? 'Region measurements' : 'Enclosing function measurements'));
    const table = el('table');
    const head = el('thead'), row = el('tr');
    for (const name of viewSnapshot ? ['Metric card', 'Snapshot value'] : ['Metric card', 'Before', 'After', 'Change']) { const cell = el('th', '', name); cell.scope = 'col'; row.append(cell); }
    head.append(row); table.append(head);
    let body = el('tbody');
    const nodes = own.map(id => byId.get(id)).filter(Boolean).sort((left, right) =>
      left.properties.observation.subject.localeCompare(right.properties.observation.subject) || metricId(left).localeCompare(metricId(right)));
    const subjects = new Set(nodes.map(node => node.properties.observation.subject));
    let previousSubject = null;
    for (const node of nodes) {
      if (subjects.size > 1 && node.properties.observation.subject !== previousSubject) {
        if (body.childElementCount) table.append(body);
        body = el('tbody');
        const group = el('tr'), heading = el('th', '', node.properties.observation.subject);
        heading.colSpan = viewSnapshot ? 2 : 4; heading.scope = 'rowgroup'; group.append(heading); body.append(group);
        previousSubject = node.properties.observation.subject;
      }
      const metric = metricId(node), observation = node.properties.observation;
      const row = el('tr'), label = el('td');
      label.append(cardCallout(metric, metricCards.get(metric)?.title || metric));
      row.append(label);
      for (const side of viewSnapshot ? ['after'] : ['before', 'after']) {
        const sample = observation[side], cell = el('td', 'metric-value');
        cell.append(el('span', '', describeValue(sample)));
        if (sample.reason) cell.append(detail('Reason', el('p', 'unknown-text', sample.reason)));
        row.append(cell);
      }
      if (!viewSnapshot) row.append(el('td', '', changeText(node)));
      body.append(row);
    }
    table.append(body); host.append(table);
    return host;
  }

  function counterpart(region, side) {
    if (side === region.side) return { start: region.start_line, end: region.end_line, note: region.start_line ? 'Selected recorded region' : 'File-level context; no source range was reported' };
    if (viewComparison) {
      for (const item of regionFindings(region)) {
        const location=item.comparison?.[side]?.[0];
        if (location) return {start:location.start_line,end:location.end_line,note:'Matched local observation in the other recorded view'};
      }
    }
    for (const id of [...region.metrics, ...region.context_metrics]) {
      const node = byId.get(id), value = node?.properties.observation;
      const location = node?.properties[side + '_location'];
      if (location && value.before.status === 'measured' && value.after.status === 'measured') {
        return { start: location.start_line, end: location.end_line, note: region.metrics.length ? 'Matched recorded subject' : 'Matched enclosing function; not an exact branch mapping' };
      }
    }
    for (const impact of region.impact) {
      if (impact.kind === 'changed_function' && impact.function[side]) {
        return { start: impact.function[side].start_line, end: impact.function[side].end_line, note: 'Matched function from recorded impact analysis' };
      }
    }
    return { start: null, end: null, note: 'Opposite revision; no reliable matching region was established' };
  }

  function sourceViewer(region) {
    const host = el('section');
    const toolbar = el('div', 'source-toolbar'), body = el('div'), caption = el('p', 'source-caption');
    let side = region.side, full = false;
    const before = button('Before', () => { side = 'before'; full = false; update(); });
    const after = button('After', () => { side = 'after'; full = false; update(); });
    const diff = button('Recorded change diff', () => { side = 'diff'; update(); });
    const expand = button('Show full file', () => { full = !full; update(); });
    if (viewSnapshot) { before.hidden = true; diff.hidden = true; after.textContent = 'View snapshot'; side = 'after'; }
    if (viewComparison) { diff.hidden=true;before.textContent='Before: '+viewComparison.from.view;after.textContent='After: '+viewComparison.to.view; }
    toolbar.append(before, after, diff, expand);
    host.append(toolbar, body, caption);
    function update() {
      before.setAttribute('aria-pressed', String(side === 'before'));
      after.setAttribute('aria-pressed', String(side === 'after'));
      diff.setAttribute('aria-pressed', String(side === 'diff'));
      body.replaceChildren();
      expand.hidden = side === 'diff';
      if (side === 'diff') {
        const change = changes.get(region.change);
        body.append(el('pre', 'source-code', change?.diff ? change.diff.replace(/\x1b\[[0-9;]*m/g, '') : 'Recorded diff unavailable: ' + (change?.diff_status || 'not supplied')));
        caption.textContent = 'Entire selected change diff, not limited to this region. Unchanged caller context may have no hunk.';
        return;
      }
      const snapshot = snapshots.get(region.change + '\0' + region.path);
      const source = snapshot?.[side];
      if (!source || source.status !== 'present') {
        body.append(el('p', 'empty', source?.status === 'absent' ? 'This file is absent in this recorded revision.' : source?.reason || 'Recorded source was not exported. No working-copy bytes are substituted.'));
        expand.hidden = true;
        caption.textContent = viewComparison ? `${side==='before'?'Before':'After'} view ${viewComparison[side==='before'?'from':'to'].view}` : viewSnapshot ? `Recorded view ${viewSnapshot.view} @ ${viewSnapshot.state}` : `${side} snapshot · change ${region.change}`;
        return;
      }
      const range = counterpart(region, side);
      const lines = source.text.split('\n');
      const start = full || !range.start ? 1 : Math.max(1, range.start - 6);
      const end = full ? lines.length : Math.min(lines.length, Math.max(start + 30, Math.min((range.end || start) + 6, start + 399)));
      const code = el('div', 'source-code');
      code.setAttribute('aria-label', `${side} recorded source, lines ${start} through ${end}`);
      for (let i = start; i <= end; i++) {
        const line = el('div', 'source-line' + (range.start && i >= range.start && i <= range.end ? ' highlight' : ''));
        const number = el('span', 'line-number', i); number.setAttribute('aria-hidden', 'true');
        line.append(number, el('span', 'line-text', lines[i - 1] || ' ')); code.append(line);
      }
      body.append(code);
      expand.textContent = full ? 'Show region context' : 'Show full file';
      const compared=comparedFiles.get(region.change), pin=viewComparison?.[side==='before'?'from':'to'];
      caption.textContent = viewComparison ? `${side==='before'?'Before':'After'} ${pin.view}: ${compared?.[side+'_path'] || region.path}, lines ${start}–${end} of ${lines.length}. State ${pin.state}. ${range.note}.` : viewSnapshot ? `Recorded view ${viewSnapshot.view}: lines ${start}–${end} of ${lines.length}. State ${viewSnapshot.state}. No working-copy bytes or before revision.` : `Recorded ${side}: lines ${start}–${end} of ${lines.length}. ${range.note}. ${snapshot.reviewed_root ? 'Selected source file.' : 'Caller context only, not an additional reviewed root.'}`;
    }
    update();
    return host;
  }

  function impactEvidence(region) {
    const host = el('section');
    for (const item of region.impact) {
      const row = el('article', 'impact-reference');
      if (item.kind === 'changed_function') {
        row.append(el('h3', '', title(item.function.change) + ' function'), el('code', '', item.function.id));
        row.append(el('p', '', item.function.signature_changed ? 'Declared signature changed.' : 'No declared signature change recorded.'));
      } else {
        const call = item.call;
        row.append(el('h3', '', title(item.kind)), el('code', '', call.caller + ' → ' + call.callee));
        const binding = call.target_present ? (call.visibility_compatible ? 'Bound direct source reference' : 'Access-incompatible reference') : 'Reference to a removed target';
        row.append(el('p', '', `${binding}. ${call.binding}.`));
        row.append(el('p', 'muted', `${call.location.path}:${call.location.start_line} · declaration ${call.declaration.path}:${call.declaration.start_line}`));
        if (call.configuration_dependent) row.append(el('p', 'muted', 'Configuration-dependent source evidence; active compiler configuration is not established.'));
      }
      host.append(row);
    }
    return host;
  }

  function selection(region, pass) {
    const host = el('div');
    host.append(el('h2', 'source-path', region.path), el('p', 'region-meta', `${location(region)} · ${regionName(region)} · ${viewComparison ? viewComparison.from.view+' → '+viewComparison.to.view : viewSnapshot ? 'view ' + viewSnapshot.view : 'change ' + region.change.slice(0, 12)}`));
    const relevant = regionFindings(region).filter(finding => inPass(finding, pass) && (pass !== 'slop' || lifecycleMatches(finding)));
    for (const finding of relevant) host.append(findingBlock(finding.number));
    if (pass === 'risk') host.append(impactEvidence(region));
    const measurements = metricTable(region);
    const metricCount = region.metrics.length || region.context_metrics.length;
    if (metricCount) {
      if (pass === 'slop' && state.filter !== 'concerns') host.append(measurements);
      else host.append(detail(`${metricCount} measurements & metric cards`, measurements));
    }
    host.append(sourceViewer(region));
    if (relevant.length) showCard(relevant[0].finding.requirement_or_rule.id);
    return host;
  }

  function lifecycleMatches(item) {
    if (!viewComparison || !['new','existing','resolved','uncertain'].includes(state.filter)) return true;
    return state.filter === 'uncertain' ? ['changed','unknown'].includes(item.comparison?.status) : item.comparison?.status === state.filter;
  }
  function eligible(region) {
    if (state.pass === 'risk') return region.impact.length || regionFindings(region).some(finding => inPass(finding, 'risk'));
    if (viewComparison && ['new','existing','resolved','uncertain'].includes(state.filter)) return regionFindings(region).some(lifecycleMatches);
    if (state.filter === 'concerns') return regionFindings(region).some(finding => inPass(finding, 'slop'));
    if (state.filter === 'changed') return region.metrics.some(id => changed(byId.get(id)));
    return region.metrics.length > 0 || region.findings.length > 0;
  }
  function matches(region) {
    if (!state.query) return true;
    const metrics = [...region.metrics, ...region.context_metrics].map(id => byId.get(id)).filter(Boolean).map(node => metricId(node) + ' ' + (metricCards.get(metricId(node))?.title || ''));
    const text = [region.path, ...region.subjects, ...metrics, numberText(region), ...regionFindings(region).map(item => item.finding.summary + ' ' + item.finding.requirement_or_rule.id), ...region.impact.map(item => item.function?.id || (item.call.caller + ' ' + item.call.callee))].join(' ').toLowerCase();
    return text.includes(state.query.toLowerCase());
  }
  function renderNavigation() {
    const host = $('files'); host.replaceChildren();
    if (state.pass === 'conformance') {
      $('navigation-title').textContent = viewConformance?.scope ? 'View intents' : 'Intents and criteria';
      const query=state.query.toLowerCase();
      const groups=Array.from(intentCatalog.values()).map(intent=>{
        const ownMatch=!query || [intent.id,intent.human_key,intent.title,intent.status,intent.view].join(' ').toLowerCase().includes(query);
        const criteria=intent.criteria.filter(item=>ownMatch || (item.text+' '+item.id+' '+item.navigation_findings.map(number=>'F'+number).join(' ')).toLowerCase().includes(query));
        return {...intent,criteria,visible:ownMatch || criteria.length>0};
      }).filter(intent=>intent.visible).sort((a,b)=>Number(b.scoped)-Number(a.scoped) || a.human_key.localeCompare(b.human_key,undefined,{numeric:true}));
      const selected=groups.find(intent=>intent.id===state.intent) || groups.find(intent=>intent.criteria.some(item=>item.id===state.criterion)) || groups[0];
      state.intent=selected?.id || null;
      if(!selected?.criteria.some(item=>item.id===state.criterion))state.criterion=selected?.criteria[0]?.id || null;
      let contextHeading=false;
      for (const intent of groups) {
        if(viewConformance?.scope && !intent.scoped && !contextHeading) {host.append(el('h3','file-title','Change-linked context (not view assignment)'));contextHeading=true;}
        const group=el('section','intent-group');group.dataset.intentGroup=intent.id;
        const own=button('',()=>{state.intent=intent.id;state.criterion=intent.criteria[0]?.id || null;state.criterionRegion=null;closeMobileFiles();render();},'region-button intent-button');
        own.dataset.intent=intent.id;own.dataset.active=String(intent.id===state.intent);
        own.setAttribute('aria-current',String(intent.id===state.intent && !state.criterion));
        own.append(el('strong','',intent.human_key),el('span','intent-name',intent.title),el('small','',`${intent.status || 'recorded context'} · ${intent.criteria.length} criteria${intent.scoped?' · view '+intent.view:' · change-linked only'}`));
        group.append(own);
        const children=el('div','intent-criteria');
        for (const item of intent.criteria) {
          const control = button('', () => { state.intent=intent.id;state.criterion = item.id; state.criterionRegion = null; closeMobileFiles(); render(); }, 'region-button criterion-button');
          control.dataset.criterion=item.id;
          control.setAttribute('aria-current', String(item.id === state.criterion));
          control.append(el('strong', '', item.text), el('small', '', `Review: ${item.review_assessment} · ${item.navigation_findings.length} related findings`));
          children.append(control);
        }
        group.append(children);host.append(group);
      }
      if (!groups.length) host.append(el('p', 'empty', intentCatalog.size ? 'No intents or criteria match this search.' : viewConformance?.scope ? 'No intents scoped to view '+viewConformance.scope.view+'.' : 'No governing intents resolved.'));
      const criteria=groups.reduce((count,intent)=>count+intent.criteria.length,0);
      $('result-count').textContent = `${groups.length} intents · ${criteria} criteria`;
      return;
    }
    $('navigation-title').textContent = 'Files & regions';
    const visible = regions.filter(region => eligible(region) && matches(region));
    if (!visible.some(region => region.id === state.region)) state.region = visible.find(region => region.findings.length)?.id || visible[0]?.id || null;
    const files = new Map();
    for (const region of visible) {
      const key = region.path + '\0' + region.change;
      if (!files.has(key)) files.set(key, []);
      files.get(key).push(region);
    }
    for (const items of files.values()) {
      const group = el('div', 'file-group');
      const compared=comparedFiles.get(items[0].change);
      group.append(el('h3', 'file-title', compared ? `${compared.before_path || '(absent)'} → ${compared.after_path || '(absent)'} · ${title(compared.status)}` : items[0].path + (report.selected_roots.length > 1 ? ' · ' + items[0].change.slice(0, 8) : '')));
      for (const region of items) {
        const control = button('', () => { state.region = region.id; closeMobileFiles(); render(); }, 'region-button');
        control.dataset.region = region.id;
        control.setAttribute('aria-current', String(region.id === state.region));
        control.append(el('strong', '', regionName(region)), el('small', '', location(region) + (region.findings.length ? ' · ' + numberText(region) : '')));
        group.append(control);
      }
      host.append(group);
    }
    if (!visible.length) host.append(el('p', 'empty', state.query ? 'No regions match this search.' : 'No regions in this view.'));
    const fileLabel = report.selected_roots.length > 1 ? (files.size === 1 ? 'file snapshot' : 'file snapshots') : (files.size === 1 ? 'file' : 'files');
    $('result-count').textContent = `${files.size} ${fileLabel} · ${visible.length} ${visible.length === 1 ? 'region' : 'regions'}`;
  }

  function renderAuditOverview() {
    const pass = passes.get('anti_slop');
    const overview = $('audit-overview'); overview.replaceChildren(); overview.className = 'overview';
    const count = graph.findings.filter(finding => inPass(finding, 'slop')).length;
    const resolved=graph.findings.filter(item=>inPass(item,'slop') && item.comparison?.status==='resolved').length;
    overview.append(el('h2', '', viewComparison ? `${count-resolved} current audit observation(s) · ${resolved} resolved` : `${count} audit finding${count === 1 ? '' : 's'}`));
    if (viewComparison) overview.append(comparisonOverview());
    overview.append(el('p', 'region-meta', 'Local analysis: atomic-semantic + atomic-audit. No AI execution or network requests.'));
    overview.append(cardCoverage());
    const checks = graph.nodes.filter(node => node.kind === 'check');
    const completed = checks.filter(node => node.properties.status === 'completed').length;
    const skipped = checks.filter(node => node.properties.status === 'skipped').length;
    const errors = checks.filter(node => node.properties.status === 'error').length;
    const coverage = el('div'); coverage.append(el('p', '', pass.detail), el('pre', '', data.audit_cards.map(card => {
      const own = checks.filter(node => node.properties.rule === card.id);
      return `${card.title}\n${own.filter(node => node.properties.status === 'completed').length} completed; ${own.filter(node => node.properties.status === 'skipped').length} skipped; ${own.filter(node => node.properties.status === 'error').length} errors\n${data.patterns[card.id]}`;
    }).join('\n\n')));
    overview.append(detail(`Audit coverage: ${completed} completed · ${skipped} skipped · ${errors} errors`, coverage));
  }

  function renderSlop() {
    const pass = passes.get('anti_slop');
    const host = $('slop-selection'); host.replaceChildren();
    const region = regions.find(region => region.id === state.region);
    if (region) host.append(selection(region, 'slop'));
    else {
      const empty = el('div', 'empty', state.query ? 'No matching source region. Clear the search to return to the review.' : pass.status === 'not_applicable' ? pass.detail : 'No numbered findings in this view. That does not establish a clean-code verdict.');
      empty.append(button('Browse changed metrics', () => { state.query = ''; $('search').value = ''; state.filter = 'changed'; $('region-filter').value = 'changed'; render(); }));
      host.append(empty);
    }
  }

  function comparisonOverview() {
    const host=el('div'), direction=el('div','comparison-direction');
    for (const [index,[label,pin]] of [['From · before',viewComparison.from],['To · after',viewComparison.to]].entries()) {
      if (index) direction.append(el('span','direction-arrow','→'));
      const side=el('div');side.append(el('small','',label),el('strong','',pin.view),el('small','',`state ${pin.state.slice(0,12)} · filter ${pin.membership_hash.slice(0,12)}`));direction.append(side);
    }
    host.append(direction);
    const files=el('div','file-comparisons'), table=el('table'), head=el('thead'), heading=el('tr'),body=el('tbody');
    for (const title of ['File state','Before path','After path','Content']) { const th=el('th','',title);th.scope='col';heading.append(th); }
    head.append(heading);table.append(head,body);
    for (const file of viewComparison.files) { const row=el('tr');row.dataset.fileComparison=file.source_ref;row.append(el('td','',title(file.status)),el('td','source-path',file.before_path || '(absent)'),el('td','source-path',file.after_path || '(absent)'),el('td','',title(file.content_status)));body.append(row); }
    files.append(table);host.append(detail(`Recorded file comparison (${viewComparison.files.length})`,files));
    host.append(el('p','muted','New/existing/resolved/changed/unknown describe local rule observations, not whole-program behavioral proof. Missing source coverage is never a resolution.'));
    return host;
  }

  function cardCoverage() {
    const host = el('div', 'card-coverage');
    const table = (label, headings) => {
      const section = el('section'); section.append(el('h3', '', label));
      const grid = el('table'), head = el('thead'), row = el('tr'), body = el('tbody');
      for (const heading of headings) { const cell = el('th', '', heading); cell.scope = 'col'; row.append(cell); }
      head.append(row); grid.append(head, body); section.append(grid); host.append(section);
      return body;
    };
    const auditBody = table('Audit cards — local rule coverage', ['Card', 'Coverage', 'Checks', viewComparison ? 'New / existing / resolved / other' : 'Findings']);
    for (const card of data.audit_cards) {
      const checks = graph.nodes.filter(node => node.kind === 'check' && node.properties.rule === card.id);
      const completed = checks.filter(node => node.properties.status === 'completed').length;
      const skipped = checks.filter(node => node.properties.status === 'skipped').length;
      const errors = checks.filter(node => node.properties.status === 'error').length;
      const status = errors ? 'Error' : completed ? (skipped ? 'Partial' : 'Checked') : skipped ? 'Skipped' : 'Not evaluated';
      const findings = graph.findings.filter(item => item.finding.requirement_or_rule.id === card.id).length;
      const row = el('tr'); row.dataset.cardId = card.id; row.dataset.kind = 'audit';
      const name = el('td'); name.append(cardCallout(card.id, card.title));
      const own=graph.findings.filter(item=>item.finding.requirement_or_rule.id===card.id);
      const totals=viewComparison ? ['new','existing','resolved','other'].map(status=>own.filter(item=>status==='other' ? !['new','existing','resolved'].includes(item.comparison?.status) : item.comparison?.status===status).length).join(' / ') : String(findings);
      row.append(name, el('td', '', status), el('td', '', `${completed} completed · ${skipped} skipped · ${errors} errors`), el('td', '', totals));
      auditBody.append(row);
    }
    const metricBody = table('Metric cards — all recorded observations', ['Card', 'Coverage', viewSnapshot ? 'Recorded snapshot' : 'Before / after', 'Observations']);
    for (const [id, card] of metricCards) {
      const nodes = graph.nodes.filter(node => node.kind === 'metric' && metricId(node) === id);
      const sides = (viewSnapshot ? ['after'] : ['before', 'after']).map(side => {
        const measured = nodes.filter(node => node.properties.observation[side]?.status === 'measured').length;
        const absent = nodes.filter(node => node.properties.observation[side]?.status === 'not_applicable').length;
        return { measured, absent, unavailable: nodes.length - measured - absent };
      });
      const measured = sides.some(side => side.measured), unavailable = sides.some(side => side.unavailable);
      const status = !nodes.length ? 'Not evaluated' : unavailable ? (measured ? 'Partial' : 'Unavailable') : measured ? 'Measured' : 'Not applicable';
      const row = el('tr'); row.dataset.cardId = id; row.dataset.kind = 'metric';
      const name = el('td'); name.append(cardCallout(id, card.title));
      row.append(name, el('td', '', status), el('td', '', sides.map(side => `${side.measured} measured · ${side.absent} N/A · ${side.unavailable} unavailable`).join(' / ')), el('td', '', String(nodes.length)));
      metricBody.append(row);
    }
    host.append(el('p', 'muted', 'Checked means the registered local rules ran, not that the code is defect-free. Unsupported and unavailable cases are never passing results. Counts cover the exact ' + (viewSnapshot ? 'recorded view snapshot.' : 'selected recorded changes.')));
    const panel = detail(`All cards: ${data.audit_cards.length} audits · ${data.metric_cards.length} metrics`, host);
    panel.id = 'card-coverage';
    return panel;
  }

  function renderConformanceOverview() {
    const host = $('conformance-overview'); host.replaceChildren(); host.className = 'overview';
    const pass = passes.get('intent_conformance');
    const heading = el('h2', '', 'Intent conformance '); heading.append(badge(title(pass.status), pass.status));
    host.append(heading, el('p', 'region-meta', pass.detail), el('p', '', `${graph.criteria.length} scoped acceptance criterion/criteria`));
    if(viewConformance?.scope)host.append(el('p','',`${viewConformance.intents.length} intents explicitly scoped to view ${viewConformance.scope.view}. Scope assignment is not change governance or acceptance approval.`));
    if (viewComparison) {
      const conformance=viewComparison.conformance;
      const changes=conformance?.changes || [];
      host.append(el('p','',`${changes.filter(change=>change.direction==='added_to_after').length} recorded change(s) added to To · ${changes.filter(change=>change.direction==='removed_from_after').length} removed from From · ${(conformance?.criteria || []).length} criterion/criteria in scope`));
      const evidence=el('div');
      for(const change of changes) evidence.append(el('p','',`${title(change.direction)} · ${change.hash} · governing intent: ${change.intent_context.status}`));
      for(const note of conformance?.diagnostics || []) evidence.append(el('p','muted',note));
      if(evidence.childElementCount)host.append(detail('Recorded membership and governance evidence',evidence));
    }
    else if(viewConformance?.diagnostics.length)host.append(detail('View intent scope notes',el('pre','',viewConformance.diagnostics.join('\n'))));
  }

  function renderCriteria() {
    const host = $('criteria'); host.replaceChildren();
    const intent=intentCatalog.get(state.intent);
    const criterion = graph.criteria.find(item => item.id === state.criterion);
    if(intent) {
      host.append(el(criterion?'p':'h2',criterion?'region-meta':'',`${intent.human_key} · ${intent.title}`));
      host.append(el('p','region-meta',intent.scoped?`View-scoped intent · ${intent.view} · ${intent.status} · ${intent.source_kind}`:'Recorded change-linked context; no explicit target-view assignment is implied.'));
      if(intent.definition_error)host.append(el('p','empty',intent.definition_error));
      if(!criterion) {host.append(el('p','empty','No acceptance criteria are available for this scoped intent. Define criteria on the intent to review them here; no acceptance verdict is inferred.'));return;}
    }
    if (!criterion) { host.append(el('p', 'empty', 'No scoped criterion assessment is available. File overlap and author completion claims are not acceptance evidence.')); return; }
    const article = el('article', 'criterion'), heading = el('div', 'criterion-title');
    heading.append(badge('Review: ' + title(criterion.review_assessment), criterion.review_assessment), badge('Author declared: ' + criterion.declared_status, 'declared'));
    article.append(heading, el('h2', '', criterion.text), el('small', '', `Criterion ${criterion.id}`), el('small', '', `Intent ${criterion.intent}`));
    article.append(detail('Criteria source and authentication', el('pre', '', viewConformance ? `Definition hash: ${criterion.source_hash}\nRecorded view definitions are preferred; any current-vault fallback is labelled context only. Recorded author verification claims are not independently re-executed or authenticated acceptance proof.` : `Current source hash: ${criterion.source_hash}\nCurrent intent definitions are source-hashed context, not authenticated historical criteria. A report-local judgment is not independent signed approval.`)));
    if(viewConformance) {
      const compared=viewConformance.criteria.find(item=>item.id===criterion.id);
      for(const [label,declaration] of [['From · before',compared?.before],[viewSnapshot?'Recorded view · '+viewSnapshot.view:'To · after',compared?.after],['Current vault · context only',compared?.current_context]]) {
        if(!declaration)continue;
        const section=el('section','criterion-declaration');
        section.append(el('h3','',label),el('p','',declaration.text),el('p','region-meta',`Author declared: ${declaration.status} · ${declaration.source_kind} · ${declaration.view || 'not a pinned historical view'} · ${declaration.path} · ${declaration.source_hash}`));
        if(declaration.verified_by || declaration.evidence)section.append(el('p','',`Declared verifier: ${declaration.verified_by || 'not supplied'} · Evidence claim (not executed): ${declaration.evidence || 'not supplied'}`));
        if(declaration.verifications?.length || declaration.required_kinds?.length)section.append(detail('Recorded verification claims — not independently authenticated or executed',el('pre','',JSON.stringify({required_kinds:declaration.required_kinds,verifications:declaration.verifications},null,2))));
        for(const task of declaration.tasks) {
          section.append(el('p','',`Task ${task.id} · ${task.status}: ${task.text}`));
          const links=el('div','callouts');
          for(const file of task.files) {
            const references=file.comparisons.flatMap(file=>regions.filter(region=>region.change===file.source_ref));
            if(!references.length)links.append(el('span','muted',`${file.path}: ${title(file.coverage)}`));
            else links.append(button(file.path+' · recorded file evidence',()=>{state.criterionRegion=references[0].id;renderCriteria();},'card-callout'));
          }
          section.append(links);
        }
        article.append(section);
      }
    }
    host.append(article);
    for (const number of criterion.findings) {
      host.append(findingBlock(number));
      const links = el('div', 'callouts');
      for (const region of regions.filter(region => region.findings.includes(number))) links.append(button(region.path + ' · ' + location(region), () => { state.criterionRegion = region.id; renderCriteria(); }, 'card-callout'));
      host.append(links);
    }
    if (!criterion.findings.length) host.append(el('p', 'empty', 'No numbered findings are mapped to this criterion. This is not an automatic passing assessment.'));
    if(viewConformance) {
      const related=graph.findings.filter(finding=>finding.related_criteria.includes(criterion.id) && !criterion.findings.includes(finding.number));
      if(related.length) {
        const body=el('div');body.append(el('p','muted','Engineering findings in declared task files are review context, not inferred criterion failures.'));
        for(const finding of related) body.append(findingBlock(finding.number));
        host.append(detail(`Related engineering findings (${related.length}) — not acceptance verdicts`,body));
      }
    }
    const source = regions.find(region => region.id === state.criterionRegion);
    if (source) host.append(detail('Source-region measurements & cards', metricTable(source)), sourceViewer(source));
  }

  function renderRiskOverview() {
    const host = $('risk-overview'); host.replaceChildren(); host.className = 'overview';
    if (!risk) { host.append(el('p', 'empty', 'No risk assessment was supplied for this scope.')); return; }
    const assessment = risk.assessment;
    const heading = el('h2', 'outcome'); heading.append(el('span', '', viewSnapshot ? 'Change risk not assessed — view snapshot' : assessment.rating === 'not_applicable' ? 'Code risk not applicable' : title(assessment.rating) + ' estimated review risk'), document.createTextNode(' '), badge(title(assessment.coverage) + ' coverage', assessment.coverage));
    host.append(heading, el('p', '', `Confidence: ${title(assessment.confidence)}. ${assessment.rationale}`));
    const rules = el('div');
    for (const rule of risk.model?.rules || []) {
      const row = el('div', 'risk-rule'); row.append(badge(title(rule.level), rule.level), document.createTextNode(' ' + rule.reason), cardCallout(rule.id, rule.id));
      rules.append(row);
    }
    const lead = risk.model?.rules[0];
    if (lead) host.append(el('p', '', lead.reason));
    if (rules.childElementCount) host.append(detail(`Triggered risk rules (${rules.childElementCount})`, rules));
    const facts = risk.model?.facts;
    if (facts) host.append(definition([
      ['Changed functions', `${shown(facts.production_functions)} production; ${shown(facts.test_support_functions)} test/support`],
      ['Observed current bound consumers', `${shown(facts.observed_production_callers)} production; ${shown(facts.observed_test_support_callers)} test/support`],
      ['Production consumers of modified existing declarations', shown(facts.existing_production_callers)],
      ['Unresolved expressions', `${shown(facts.unresolved_references)} — excluded from bound-caller counts`]
    ]));
    const checks = risk.impact.reduce((count, report) => count + report.checked_call_contracts, 0);
    host.append(el('p', 'muted', `${checks} source call-contract checks. Availability/access/arity checks do not prove type, runtime or executed-test correctness.`));
    host.append(detail(`Coverage limits (${assessment.limitations.length})`, el('pre', '', assessment.limitations.join('\n'))));
  }

  function renderRisk() {
    const selected = $('risk-selection'); selected.replaceChildren();
    const region = regions.find(region => region.id === state.region);
    if (region) selected.append(selection(region, 'risk'));
    else selected.append(el('p', 'empty', 'No located impact evidence in this selection. Missing or unresolved relationships are not fabricated callers.'));
  }

  function renderSummary() {
    const actions = $('summary-actions');
    for (const [section, label] of [['slop', 'Review audit findings'], ['conformance', 'Review conformance'], ['risk', 'Review blast radius']]) {
      const control = button(label, () => { activate(section); $('tab-' + section).focus(); });
      control.dataset.section = section; actions.append(control);
    }
    // The report is immutable. Build report-wide tables once instead of
    // rebuilding them whenever a different file or region is selected.
    renderAuditOverview();
    renderConformanceOverview();
    renderRiskOverview();
  }

  function render() {
    for (const name of sections) {
      const active = state.pass === name;
      $('tab-' + name).setAttribute('aria-selected', String(active));
      $('tab-' + name).tabIndex = active ? 0 : -1;
      $('panel-' + name).hidden = !active;
    }
    const summary = state.pass === 'summary';
    $('workspace').dataset.summary = String(summary);
    $('review-toolbar').hidden = summary;
    $('file-navigation').hidden = summary;
    $('card-inspector').hidden = summary;
    if (summary) return;
    $('region-filter').parentElement.hidden = state.pass !== 'slop';
    $('search-title').textContent = state.pass === 'conformance' ? 'Find an intent or criterion' : 'Find a file or region';
    $('search').placeholder = state.pass === 'conformance' ? 'Intent title, ID, criterion or F-number' : state.pass === 'risk' ? 'Caller, callee, file or F-number' : 'Path, function, metric, audit or F-number';
    $('file-navigation').setAttribute('aria-label',state.pass==='conformance'?'View intents and acceptance criteria':'Files and source regions');
    $('file-drawer-label').textContent=state.pass==='conformance'?'Choose an intent or criterion':'Choose a file or region';
    renderNavigation();
    if (state.pass === 'slop') renderSlop();
    else if (state.pass === 'conformance') renderCriteria();
    else renderRisk();
  }
  const mobileFiles = matchMedia('(max-width:700px)');
  function closeMobileFiles() { if (mobileFiles.matches) $('file-drawer').open = false; }
  function resizeNavigation() { $('file-drawer').open = !mobileFiles.matches; }
  mobileFiles.addEventListener('change', resizeNavigation);
  resizeNavigation();
  function activate(name) {
    if (state.pass in rememberedRegions) rememberedRegions[state.pass] = state.region;
    state.pass = name; state.region = rememberedRegions[name] || null; render(); window.scrollTo(0, 0);
  }
  for (const name of sections) $('tab-' + name).addEventListener('click', () => activate(name));
  document.querySelector('.pass-tabs').addEventListener('keydown', event => {
    const index = sections.indexOf(state.pass);
    const next = event.key === 'ArrowRight' ? (index + 1) % sections.length : event.key === 'ArrowLeft' ? (index + sections.length - 1) % sections.length : event.key === 'Home' ? 0 : event.key === 'End' ? sections.length - 1 : null;
    if (next === null) return;
    event.preventDefault(); activate(sections[next]); $('tab-' + sections[next]).focus();
  });
  $('search').addEventListener('input', event => { state.query = event.target.value; render(); });
  $('region-filter').addEventListener('change', event => { state.filter = event.target.value; state.region = null; render(); });
  $('card-picker').addEventListener('change', event => { if (event.target.value) showCard(event.target.value); });
  for (const [label, cards] of [['Audit cards', auditCards], ['Metric cards', metricCards], ['Triggered risk rules', riskRules]]) {
    const group = el('optgroup'); group.label = label;
    for (const [id, card] of cards) { const option = el('option', '', card.title || id); option.value = id; group.append(option); }
    $('card-picker').append(group);
  }
  const root = viewComparison ? viewComparison.source_ref.split(':').pop() : viewSnapshot ? viewSnapshot.source_ref.split(':').pop() : report.selected_roots[0] || '';
  $('report-title').textContent = viewComparison ? viewComparison.from.view+' → '+viewComparison.to.view : viewSnapshot ? 'View review · ' + viewSnapshot.view : report.selected_roots.length === 1 ? 'Review ' + root.slice(0, 12) : `Review ${report.selected_roots.length} selected changes`;
  if (viewComparison) for (const [value,label] of [['new','New audit findings'],['existing','Existing audit findings'],['resolved','Resolved audit findings'],['uncertain','Changed / unknown findings']]) { const option=el('option','',label);option.value=value;$('region-filter').append(option); }
  if (viewSnapshot) {
    $('region-filter').value = 'all';
    $('region-filter').querySelector('option[value="changed"]').disabled = true;
  }
  document.title = $('report-title').textContent + ' · Atomic';
  $('report-scope').append(document.createTextNode(graph.scope.view + ' · '), el('span', 'scope-state', `state ${graph.scope.view_state.slice(0, 12)} · `), document.createTextNode('not signed approval'));
  if (viewSnapshot) $('report-scope').append(document.createTextNode(` · filter ${viewSnapshot.membership_hash.slice(0,12)} · ${viewSnapshot.file_count} live recorded paths`));
  const slopCount = graph.findings.filter(item => inPass(item, 'slop')).length;
  $('slop-status').textContent = `${slopCount} ${slopCount === 1 ? 'finding' : 'findings'} · ${title(passes.get('anti_slop').status)}`;
  $('conformance-status').textContent = title(passes.get('intent_conformance').status);
  $('risk-status').textContent = risk ? `${title(risk.assessment.rating)} · ${title(risk.assessment.coverage)}` : 'Unavailable';
  $('provenance').append(el('h2', '', 'Recorded review scope'), el('pre', '', JSON.stringify(graph.scope, null, 2)), el('h3', '', 'Scope notes'), el('pre', '', [...graph.diagnostics, ...report.warnings].join('\n')), el('p', '', 'Additional source snapshots are bounded exports from this recorded scope. Working-copy bytes are never substituted. Keep this artifact private if its source/evidence is sensitive.'));
  if (typeof DecompressionStream !== 'function') $('download').textContent = 'Download JSON.gz';
  $('download').addEventListener('click', async () => {
    const control = $('download'), label = control.textContent;
    control.disabled = true; control.textContent = 'Preparing JSON…';
    try {
      const bytes = Uint8Array.from(atob(data.report_archive.data), character => character.charCodeAt(0));
      let blob = new Blob([bytes], { type: 'application/gzip' }), extension = '.json.gz';
      if (typeof DecompressionStream === 'function') {
        blob = await new Response(blob.stream().pipeThrough(new DecompressionStream('gzip'))).blob();
        extension = '.json';
      }
      const url = URL.createObjectURL(blob);
      const link = el('a'); link.href = url; link.download = 'atomic-review-' + (root.slice(0, 12).replace(/[^a-z0-9]/gi, '') || 'report') + extension;
      document.body.append(link); link.click(); link.remove(); setTimeout(() => URL.revokeObjectURL(url), 0);
    } catch (error) {
      control.title = 'Could not restore JSON archive: ' + error.message;
      control.textContent = 'Download failed';
      control.disabled = false;
      return;
    }
    control.disabled = false; control.textContent = label;
  });
  renderSummary();
  render();
})();
