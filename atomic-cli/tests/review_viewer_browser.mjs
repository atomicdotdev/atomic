// Optional browser regression without npm dependencies.
// node review_viewer_browser.mjs REPORT.html CHROME_BINARY SCRATCH_PARENT
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { StringDecoder } from 'node:string_decoder';
import assert from 'node:assert/strict';
import { gzipSync, gunzipSync } from 'node:zlib';

const artifact = resolve(process.argv[2]);
const chrome = process.argv[3] || process.env.CHROME;
assert(chrome, 'Supply a Chrome/Chromium executable; the test never uses an existing browser profile.');
const directory = mkdtempSync(join(process.argv[4] || tmpdir(), 'atomic-review-browser-'));
const child = spawn(chrome, ['--headless=new', '--remote-debugging-pipe', '--disable-gpu', '--disable-extensions', '--disable-background-networking', '--disable-component-update', '--no-first-run', '--no-default-browser-check', '--user-data-dir=' + join(directory, 'profile'), 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe', 'pipe', 'pipe'] });
let stderr = '', input = '', serial = 0;
const decoder = new StringDecoder('utf8'), pending = new Map(), listeners = new Set();
child.stderr.on('data', chunk => { stderr += chunk; });
child.stdio[4].on('data', chunk => {
  input += decoder.write(chunk);
  for (let end; (end = input.indexOf('\0')) >= 0;) {
    const frame = input.slice(0, end); input = input.slice(end + 1);
    if (!frame) continue;
    const message = JSON.parse(frame);
    if (message.id && pending.has(message.id)) {
      const request = pending.get(message.id); pending.delete(message.id); clearTimeout(request.timeout);
      if (message.error) request.reject(new Error(JSON.stringify(message.error))); else request.resolve(message.result);
    }
    for (const listener of listeners) listener(message);
  }
});
function send(method, params = {}, sessionId) {
  return new Promise((resolve, reject) => {
    const id = ++serial;
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error('CDP timed out: ' + method + '\n' + stderr)); }, 30000);
    pending.set(id, { resolve, reject, timeout });
    child.stdio[3].write(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }) + '\0');
  });
}
function event(method, sessionId) {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => { listeners.delete(listener); reject(new Error('Missing browser event: ' + method)); }, 30000);
    const listener = message => {
      if (message.method === method && message.sessionId === sessionId) { clearTimeout(timeout); listeners.delete(listener); resolve(message.params); }
    };
    listeners.add(listener);
  });
}
child.once('error', error => { for (const request of pending.values()) { clearTimeout(request.timeout); request.reject(error); } });
child.once('exit', () => { for (const request of pending.values()) { clearTimeout(request.timeout); request.reject(new Error('Browser exited')); } pending.clear(); });

try {
  await send('Browser.getVersion');
  const { targetId } = await send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await send('Target.attachToTarget', { targetId, flatten: true });
  const page = (method, params) => send(method, params, sessionId);
  const evaluate = async expression => {
    const result = await page('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true, userGesture: true });
    assert(!result.exceptionDetails, JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  const errors = [], requests = [];
  listeners.add(message => {
    if (message.sessionId !== sessionId) return;
    if (message.method === 'Runtime.exceptionThrown') errors.push(message.params.exceptionDetails);
    if (message.method === 'Network.requestWillBeSent') requests.push(message.params.request.url);
  });
  await page('Page.enable'); await page('Runtime.enable'); await page('Network.enable');
  await page('Emulation.setDeviceMetricsOverride', { width: 1600, height: 1100, deviceScaleFactor: 1, mobile: false });
  const load = async path => { const loaded = event('Page.loadEventFired', sessionId); await page('Page.navigate', { url: pathToFileURL(path).href }); await loaded; };
  await load(artifact);
  assert.equal(errors.length, 0, JSON.stringify(errors));
  assert.equal(await evaluate(`document.querySelectorAll('[role="tab"]').length`), 4);
  assert(await evaluate(`document.getElementById('tab-summary').getAttribute('aria-selected')==='true' && !document.getElementById('panel-summary').hidden`));
  assert(await evaluate(`document.getElementById('review-toolbar').hidden && document.getElementById('file-navigation').hidden && document.getElementById('card-inspector').hidden`));
  assert(await evaluate(`['audit-overview','conformance-overview','risk-overview'].every(id=>document.getElementById(id).closest('[role="tabpanel"]').id==='panel-summary')`),'Report-wide overviews belong only to Summary');
  assert.equal(await evaluate(`document.querySelectorAll('#summary-actions button').length`),3);
  let image = await page('Page.captureScreenshot', { format: 'png' });
  writeFileSync(join(directory,'summary-desktop.png'),Buffer.from(image.data,'base64'));
   const expected = await evaluate(`(() => { const d=JSON.parse(document.getElementById('review-data').textContent); const r=d.view; const c=r.view_comparison?.conformance || r.view_snapshot?.conformance; return { findings:r.graph.findings.filter(f=>f.passes.includes('anti_slop')).length, riskFindings:r.graph.findings.filter(f=>f.passes.includes('blast_radius_risk')).map(f=>f.number), criteria:r.graph.criteria.length, viewIntents:c?.intents || [], intentView:c?.scope?.view || null, metrics:d.metric_cards.length, observations:r.graph.nodes.filter(n=>n.kind==='metric').length, scope:r.graph.scope.selected_roots, snapshot:r.view_snapshot?.view || null, comparison:r.view_comparison || null }; })()`);
   assert(await evaluate(`(()=>{const logo=document.querySelector('.atomic-logo');return logo?.complete && logo.naturalWidth===560 && logo.src.startsWith('data:image/png;base64,') && logo.alt==='Atomic';})()`), 'Official logo must be embedded and decoded offline');
   assert.equal(await evaluate(`getComputedStyle(document.documentElement).getPropertyValue('--canvas').trim()`),'#07041a');
   assert.equal(await evaluate(`getComputedStyle(document.documentElement).getPropertyValue('--accent').trim()`),'#ff0a8c');
   const contrasts = await evaluate(`(() => {
     const rgba = color => (color.match(/[\\d.]+/g) || []).map(Number);
     const over = (a,b) => a.slice(0,3).map((channel,index)=>channel*(a[3]??1)+b[index]*(1-(a[3]??1)));
     function background(element) { if(!element)return [7,4,26]; return over(rgba(getComputedStyle(element).backgroundColor),background(element.parentElement)); }
     const luminance = color => color.map(value=>{value/=255;return value<=.04045?value/12.92:((value+.055)/1.055)**2.4}).reduce((sum,value,index)=>sum+value*[.2126,.7152,.0722][index],0);
     return ['body','.region-meta','.overview p','a','.badge','.source-toolbar button[aria-pressed=true]'].flatMap(selector=>Array.from(document.querySelectorAll(selector)).slice(0,12).filter(node=>node.getClientRects().length).map(node=>{
       const bg=background(node), fg=over(rgba(getComputedStyle(node).color),bg), a=luminance(fg), b=luminance(bg);
       return {selector,ratio:(Math.max(a,b)+.05)/(Math.min(a,b)+.05)};
     }));
   })()`);
   assert(contrasts.every(item=>item.ratio>=4.5),'Review text must meet AA contrast: '+JSON.stringify(contrasts));
   if (expected.comparison) {
     assert(await evaluate(`document.querySelector('.comparison-direction').textContent.includes('From · before') && document.querySelector('.comparison-direction').textContent.includes('To · after')`));
     assert.equal(await evaluate(`document.querySelectorAll('[data-file-comparison]').length`),expected.comparison.files.length);
   }
  assert.equal(expected.metrics, 10);
  assert.equal(await evaluate(`document.querySelectorAll('#card-picker optgroup[label="Metric cards"] option').length`), 10);
  const localCoverage = await evaluate(`(() => {
    const d=JSON.parse(document.getElementById('review-data').textContent);
    const rows=Array.from(document.querySelectorAll('#card-coverage tr[data-card-id]'));
    return {local:d.local_only, audits:rows.filter(row=>row.dataset.kind==='audit').map(row=>row.dataset.cardId).sort(),
      metrics:rows.filter(row=>row.dataset.kind==='metric').map(row=>row.dataset.cardId).sort(),
      expectedAudits:d.audit_cards.map(card=>card.id).sort(), expectedMetrics:d.metric_cards.map(card=>'metric.'+card.metric.replaceAll('_','-')).sort(),
      totals:rows.filter(row=>row.dataset.kind==='metric').reduce((sum,row)=>sum+Number(row.cells[3].textContent),0)};
  })()`);
  assert.equal(localCoverage.local, true);
  assert.deepEqual(localCoverage.audits, localCoverage.expectedAudits);
  assert.deepEqual(localCoverage.metrics, localCoverage.expectedMetrics);
   assert.equal(localCoverage.totals, expected.observations, 'All observations must be accounted for, not just finding-related metrics');
   await evaluate(`document.getElementById('card-coverage').dataset.renderProbe='stable';document.querySelector('#summary-actions button[data-section="conformance"]').click()`);
   assert(await evaluate(`!document.getElementById('panel-conformance').hidden && document.getElementById('panel-summary').hidden`));
   await evaluate(`document.getElementById('tab-summary').click()`);
   await evaluate(`document.getElementById('card-coverage').open=true;document.querySelector('#card-coverage tr[data-kind="metric"] button').click()`);
   assert(await evaluate(`!document.getElementById('panel-slop').hidden && document.getElementById('panel-summary').hidden && !document.getElementById('card-inspector').hidden`),'Summary card links must open the visible detail inspector');
   assert(await evaluate(`document.getElementById('panel-slop').firstElementChild.id==='slop-selection' && !document.querySelector('#panel-slop .overview')`),'File views must start with the selected evidence, not the report overview');
  assert(await evaluate(`document.getElementById('card-content').textContent.includes('Definition')`));
  assert(await evaluate(`document.getElementById('card-content').textContent.includes('All recorded observations')`));
   await evaluate(`document.getElementById('card-coverage').open=false`);
   if(expected.comparison) {
     for (const value of ['new','existing','resolved','uncertain']) {
       await evaluate(`document.getElementById('region-filter').value=${JSON.stringify(value)};document.getElementById('region-filter').dispatchEvent(new Event('change',{bubbles:true}))`);
       const selected=await evaluate(`Array.from(document.querySelectorAll('#slop-selection .finding h3 .badge:last-child')).map(node=>node.textContent.toLowerCase())`);
       assert(selected.every(status=>value==='uncertain' ? ['changed','unknown'].includes(status) : status===value), 'Lifecycle filter must also filter findings in the selected region');
     }
     await evaluate(`document.getElementById('region-filter').value='concerns';document.getElementById('region-filter').dispatchEvent(new Event('change',{bubbles:true}))`);
   }
  assert(await evaluate(`document.body.textContent.includes('not signed approval')`));
   if (expected.findings) {
     const auditRendered=await evaluate(`(()=>{
       const d=JSON.parse(document.getElementById('review-data').textContent),numbers=new Set(d.view.graph.findings.filter(f=>f.passes.includes('anti_slop')).map(f=>f.number)),seen=new Set();
       for(const region of d.regions.filter(region=>region.findings.some(number=>numbers.has(number)))) {
         const control=Array.from(document.querySelectorAll('#files .region-button')).find(button=>button.dataset.region===region.id);if(!control)continue;control.click();
         for(const finding of document.querySelectorAll('#slop-selection .finding'))seen.add(Number(finding.dataset.finding));
       }
       return {seen:[...seen].sort((a,b)=>a-b),expected:[...numbers].sort((a,b)=>a-b)};
     })()`);
     assert.deepEqual(auditRendered.seen,auditRendered.expected,'Every JSON audit finding must be renderable in HTML');
      assert(await evaluate(`document.querySelectorAll('#files .region-button').length > 0`));
     const selectedRegion=await evaluate(`document.querySelector('#files .region-button[aria-current=true]').dataset.region`);
     await evaluate(`document.getElementById('tab-summary').click();document.getElementById('tab-slop').click()`);
     assert.equal(await evaluate(`document.querySelector('#files .region-button[aria-current=true]').dataset.region`),selectedRegion,'Returning from Summary must retain the selected region');
     assert.equal(await evaluate(`document.getElementById('card-coverage').dataset.renderProbe`),'stable','Summary tables must not be rebuilt on each file selection');
    assert(await evaluate(`document.querySelector('#slop-selection .finding').textContent.includes('F')`));
    await evaluate(`document.querySelector('#slop-selection .finding .card-callout').click()`);
    assert(await evaluate(`document.getElementById('card-content').textContent.includes('Supported patterns')`));
    if (await evaluate(`!!document.querySelector('.metrics-block .card-callout')`)) {
      await evaluate(`for(const item of document.querySelectorAll('#slop-selection details')){if(item.querySelector('summary').textContent.includes('measurements & metric cards'))item.open=true}`);
      await evaluate(`document.querySelector('.metrics-block .card-callout').click()`);
      assert(await evaluate(`document.getElementById('card-content').textContent.includes('Definition')`));
      await evaluate(`for(const item of document.querySelectorAll('#slop-selection details')){if(item.querySelector('summary').textContent.includes('measurements & metric cards'))item.open=false}`);
    }
    if (expected.snapshot) {
      assert(await evaluate(`document.querySelector('#slop-selection .source-toolbar button').hidden`));
      assert(await evaluate(`document.querySelectorAll('#slop-selection .source-toolbar button')[2].hidden`));
      assert(await evaluate(`document.querySelectorAll('#slop-selection .source-toolbar button')[1].textContent==='View snapshot'`));
      assert.equal(await evaluate(`document.getElementById('region-filter').value`),'all');
      assert(await evaluate(`document.querySelector('#region-filter option[value="changed"]').disabled`));
    } else {
      await evaluate(`document.querySelector('#slop-selection .source-toolbar button').click()`);
      assert(await evaluate(`document.querySelector('#slop-selection .source-toolbar button').getAttribute('aria-pressed')==='true'`));
      await evaluate(`document.querySelectorAll('#slop-selection .source-toolbar button')[1].click()`);
      assert(await evaluate(`document.querySelectorAll('#slop-selection .source-toolbar button')[1].getAttribute('aria-pressed')==='true'`));
    }
    await evaluate(`document.getElementById('search').value='NO_MATCH_FOR_REVIEW_VIEWER_TEST';document.getElementById('search').dispatchEvent(new Event('input',{bubbles:true}))`);
    assert.equal(await evaluate(`document.querySelectorAll('#files .region-button').length`), 0);
    await evaluate(`document.getElementById('search').value='';document.getElementById('search').dispatchEvent(new Event('input',{bubbles:true}))`);
    const comparable = await evaluate(`(()=>{const d=JSON.parse(document.getElementById('review-data').textContent);const nodes=new Map(d.view.graph.nodes.map(n=>[n.id,n]));for(const r of d.regions){if(!r.findings.length||r.side!=='after')continue;for(const id of r.metrics){const n=nodes.get(id);if(n?.properties.before_location&&n.properties.observation.before.status==='measured'&&n.properties.observation.after.status==='measured')return{id:r.id,start:n.properties.before_location.start_line};}}return null;})()`);
    if (comparable) {
      await evaluate(`Array.from(document.querySelectorAll('#files .region-button')).find(button=>button.dataset.region===${JSON.stringify(comparable.id)}).click();document.querySelector('#slop-selection .source-toolbar button').click()`);
      assert.equal(await evaluate(`document.querySelector('#slop-selection .source-line.highlight .line-number').textContent`), String(comparable.start));
      assert(await evaluate(`document.querySelector('#slop-selection .source-caption').textContent.includes('Matched recorded subject') || document.querySelector('#slop-selection .source-caption').textContent.includes('Matched local observation')`));
    }
  }
  await evaluate(`document.getElementById('region-filter').value='all';document.getElementById('region-filter').dispatchEvent(new Event('change',{bubbles:true}))`);
  assert.equal(await evaluate(`document.querySelectorAll('#files .region-button').length>0`), expected.observations > 0);
  await evaluate(`document.getElementById('region-filter').value='concerns';document.getElementById('region-filter').dispatchEvent(new Event('change',{bubbles:true}))`);
   await evaluate(`document.getElementById('tab-conformance').click()`);
   assert(await evaluate(`!document.getElementById('panel-conformance').hidden && document.getElementById('panel-slop').hidden`));
   if(expected.intentView)assert.equal(await evaluate(`document.getElementById('navigation-title').textContent`),'View intents');
   for(const intent of expected.viewIntents) {
     assert.equal(intent.view,expected.intentView);
     assert(await evaluate(`(()=>{const row=Array.from(document.querySelectorAll('#files .intent-button')).find(node=>node.dataset.intent===${JSON.stringify(intent.id)});return !!row && row.textContent.includes(${JSON.stringify(intent.human_key)}) && row.textContent.includes(${JSON.stringify(intent.title)});})()`),'Every scoped intent must appear by human key/title even without change governance');
     for(const query of [intent.title,intent.human_key]) {
       await evaluate(`document.getElementById('search').value=${JSON.stringify(query)};document.getElementById('search').dispatchEvent(new Event('input',{bubbles:true}))`);
       assert(await evaluate(`Array.from(document.querySelectorAll('#files .intent-button')).some(node=>node.dataset.intent===${JSON.stringify(intent.id)})`),'Intent title/key must be searchable');
     }
     await evaluate(`document.getElementById('search').value='';document.getElementById('search').dispatchEvent(new Event('input',{bubbles:true}))`);
   }
   if(expected.viewIntents.length) {
     const withoutCriteria=await evaluate(`(()=>{const d=JSON.parse(document.getElementById('review-data').textContent).view,c=d.view_comparison?.conformance || d.view_snapshot?.conformance;return c.intents.find(intent=>!d.graph.criteria.some(criterion=>criterion.intent===intent.id))?.id || null;})()`);
     if(withoutCriteria) {
       await evaluate(`Array.from(document.querySelectorAll('#files .intent-button')).find(node=>node.dataset.intent===${JSON.stringify(withoutCriteria)}).click()`);
       assert(await evaluate(`document.getElementById('criteria').textContent.includes('No acceptance criteria are available for this scoped intent')`));
     }
   }
   const firstCriterion=await evaluate(`JSON.parse(document.getElementById('review-data').textContent).view.graph.criteria[0]?.id || null`);
   const linkedFinding=await evaluate(`(()=>{const g=JSON.parse(document.getElementById('review-data').textContent).view.graph;const f=g.findings.find(f=>f.related_criteria.some(id=>g.criteria.some(c=>c.id===id)));return f?{number:f.number,criterion:f.related_criteria.find(id=>g.criteria.some(c=>c.id===id))}:null;})()`);
   if(linkedFinding) {
     await evaluate(`document.getElementById('search').value='F'+${linkedFinding.number};document.getElementById('search').dispatchEvent(new Event('input',{bubbles:true}))`);
     assert(await evaluate(`Array.from(document.querySelectorAll('#files .criterion-button')).some(node=>node.dataset.criterion===${JSON.stringify(linkedFinding.criterion)})`),'Related finding numbers must be searchable without treating them as acceptance verdicts');
     await evaluate(`document.getElementById('search').value='';document.getElementById('search').dispatchEvent(new Event('input',{bubbles:true}))`);
   }
   if(firstCriterion)await evaluate(`Array.from(document.querySelectorAll('#files .criterion-button')).find(node=>node.dataset.criterion===${JSON.stringify(firstCriterion)}).click()`);
   if (!expected.criteria) assert(await evaluate(`document.getElementById('criteria').textContent.includes(${JSON.stringify(expected.viewIntents.length?'No acceptance criteria are available for this scoped intent':'No scoped criterion assessment')})`));
   else {
    assert(await evaluate(`(()=>{const r=JSON.parse(document.getElementById('review-data').textContent).view;const criterion=r.graph.criteria[0];const text=document.getElementById('criteria').textContent;return text.includes(criterion.text)&&text.includes('Author declared: '+criterion.declared_status)&&text.includes('Review: '+criterion.review_assessment.replaceAll('_',' ').replace(/^./,c=>c.toUpperCase()));})()`));
     const nested=await evaluate(`(()=>{const row=document.querySelector('#files .criterion-button[aria-current=true]');return !!row?.closest('.intent-group')?.querySelector('.intent-button');})()`);
     assert(nested,'Criteria must be grouped beneath their intent');
     const query=await evaluate(`JSON.parse(document.getElementById('review-data').textContent).view.graph.criteria[0].text`);
     await evaluate(`document.getElementById('search').value=${JSON.stringify(query)};document.getElementById('search').dispatchEvent(new Event('input',{bubbles:true}))`);
     assert(await evaluate(`Array.from(document.querySelectorAll('#files .criterion-button')).some(node=>node.dataset.criterion===${JSON.stringify(firstCriterion)})`));
     await evaluate(`document.getElementById('search').value='';document.getElementById('search').dispatchEvent(new Event('input',{bubbles:true}))`);
     if (await evaluate(`!!document.querySelector('#criteria > .callouts button, #criteria .criterion-declaration .callouts button')`)) {
       await evaluate(`document.querySelector('#criteria > .callouts button, #criteria .criterion-declaration .callouts button').click()`);
       assert(await evaluate(`!!document.querySelector('#criteria .source-toolbar')`));
     }
   }
   assert(await evaluate(`document.documentElement.scrollWidth <= window.innerWidth`));
   image=await page('Page.captureScreenshot',{format:'png'});writeFileSync(join(directory,'conformance-desktop.png'),Buffer.from(image.data,'base64'));
  await evaluate(`document.getElementById('tab-risk').click();document.getElementById('tab-risk').focus()`);
   assert(await evaluate(`!document.getElementById('panel-risk').hidden`));
   assert(await evaluate(`document.getElementById('panel-risk').firstElementChild.id==='risk-selection' && !document.querySelector('#panel-risk .overview')`));
   assert(await evaluate(`document.getElementById('risk-overview').textContent.includes('risk')`));
   if(expected.riskFindings.length) {
     const rendered=await evaluate(`(()=>{
       const d=JSON.parse(document.getElementById('review-data').textContent),numbers=new Set(d.view.graph.findings.filter(f=>f.passes.includes('blast_radius_risk')).map(f=>f.number)),seen=new Set();
       for(const region of d.regions.filter(region=>region.findings.some(number=>numbers.has(number)))) {
         const control=Array.from(document.querySelectorAll('#files .region-button')).find(button=>button.dataset.region===region.id);if(!control)continue;control.click();
         for(const finding of document.querySelectorAll('#risk-selection .finding'))seen.add(Number(finding.dataset.finding));
       }
       return [...seen].sort((a,b)=>a-b);
     })()`);
     assert.deepEqual(rendered,expected.riskFindings.sort((a,b)=>a-b),'Every JSON risk finding must be renderable in HTML, including finding 101 and beyond');
   }
  await page('Input.dispatchKeyEvent', { type: 'keyDown', key: 'ArrowLeft', code: 'ArrowLeft', windowsVirtualKeyCode: 37 });
  await page('Input.dispatchKeyEvent', { type: 'keyUp', key: 'ArrowLeft', code: 'ArrowLeft', windowsVirtualKeyCode: 37 });
   assert.equal(await evaluate(`document.activeElement.id`), 'tab-conformance');
   for(const [key,id] of [['Home','tab-summary'],['End','tab-risk'],['ArrowRight','tab-summary'],['ArrowLeft','tab-risk']]) {
     await page('Input.dispatchKeyEvent',{type:'keyDown',key,code:key});
     await page('Input.dispatchKeyEvent',{type:'keyUp',key,code:key});
     assert.equal(await evaluate(`document.activeElement.id`),id);
   }
  await evaluate(`document.getElementById('tab-slop').click();document.getElementById('tab-slop').focus();window.scrollTo(0,0)`);
  assert(await evaluate(`document.documentElement.scrollWidth <= window.innerWidth`));
   image = await page('Page.captureScreenshot', { format: 'png' });
  writeFileSync(join(directory, 'desktop.png'), Buffer.from(image.data, 'base64'));
  const mobileReady = evaluate(`new Promise(resolve=>{const query=matchMedia('(max-width:700px)');if(query.matches)resolve(true);else query.addEventListener('change',()=>resolve(true),{once:true});})`);
  await page('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
  await mobileReady;
  assert(await evaluate(`!document.getElementById('file-drawer').open`));
  assert(await evaluate(`document.documentElement.scrollWidth <= window.innerWidth`), 'Mobile viewport must not overflow horizontally');
  image = await page('Page.captureScreenshot', { format: 'png' });
   writeFileSync(join(directory, 'mobile.png'), Buffer.from(image.data, 'base64'));
   await evaluate(`document.getElementById('tab-summary').click();document.getElementById('tab-summary').focus();window.scrollTo(0,0)`);
   assert(await evaluate(`document.documentElement.scrollWidth <= window.innerWidth && document.getElementById('review-toolbar').hidden && document.getElementById('file-navigation').hidden && document.getElementById('card-inspector').hidden`));
   image=await page('Page.captureScreenshot',{format:'png'});
   writeFileSync(join(directory,'summary-mobile.png'),Buffer.from(image.data,'base64'));
   await evaluate(`document.getElementById('tab-conformance').click();document.getElementById('tab-conformance').focus();window.scrollTo(0,0)`);
   if(expected.viewIntents.length) {
     await evaluate(`document.getElementById('file-drawer').open=true;(document.querySelector('#files .criterion-button') || document.querySelector('#files .intent-button')).click()`);
     assert(await evaluate(`!document.getElementById('file-drawer').open`),'Mobile intent/criterion selection must close the navigation drawer');
   }
   assert(await evaluate(`document.documentElement.scrollWidth <= window.innerWidth`),'Grouped conformance navigation must fit the mobile viewport');
   image=await page('Page.captureScreenshot',{format:'png'});writeFileSync(join(directory,'conformance-mobile.png'),Buffer.from(image.data,'base64'));

  // Hostile report/source content must remain inert, even in navigation labels.
  // Keep this explicitly synthetic artifact in the private scratch directory.
  const original = readFileSync(artifact, 'utf8');
  const pattern = /(<script\b[^>]*\bid="review-data"[^>]*>)([\s\S]*?)(<\/script>)/;
  const fixture = JSON.parse(original.match(pattern)[2]);
  const hostile = '</script><img id="audit-xss" src="https://example.invalid/never-fetch" onerror="globalThis.__audit_xss=1">';
  fixture.view.graph.scope.view = hostile;
   fixture.view.warnings.push(hostile);
   if(fixture.view.view_comparison) {
     fixture.view.view_comparison.from.view=hostile;
     fixture.view.view_comparison.files[0].path=hostile;
   }
   fixture.audit_cards[0].title = hostile;
   const scoped=fixture.view.view_comparison?.conformance || fixture.view.view_snapshot?.conformance;
   if(scoped?.intents?.length){scoped.intents[0].title=hostile;scoped.intents[0].human_key=hostile;}
  const reportText = gunzipSync(Buffer.from(fixture.report_archive.data,'base64')).toString('utf8');
  fixture.report_archive.data = gzipSync(reportText.slice(0,-1) + ',"precision_probe":18446744073709551615}',{level:1}).toString('base64');
  const encoded = JSON.stringify(fixture).replace(/[<>&\u2028\u2029]/g, character => '\\u' + character.charCodeAt(0).toString(16).padStart(4, '0'));
  const hostilePath = join(directory, 'hostile-fixture.html');
  writeFileSync(hostilePath, original.replace(pattern, (_, prefix, _data, suffix) => prefix + encoded + suffix));
   await load(hostilePath);
   await evaluate(`document.getElementById('tab-conformance').click()`);
  assert(await evaluate(`!document.getElementById('audit-xss') && !globalThis.__audit_xss`));
  const downloaded = await evaluate(`(async()=>{let captured,ready;const prepared=new Promise(resolve=>ready=resolve); const create=URL.createObjectURL;const click=HTMLAnchorElement.prototype.click;URL.createObjectURL=blob=>{captured=blob;ready();return create.call(URL,blob)};HTMLAnchorElement.prototype.click=function(){};document.getElementById('download').click();await prepared;const text=await captured.text();URL.createObjectURL=create;HTMLAnchorElement.prototype.click=click;return text.includes('"precision_probe":18446744073709551615');})()`);
  assert(downloaded, 'Downloads must preserve original u64 JSON text');
  assert.equal(errors.length, 0, JSON.stringify(errors));
  assert(requests.every(url => url.startsWith('file:') || url.startsWith('data:')), 'Viewer must not make external requests: ' + requests.join(', '));
  console.log(JSON.stringify({ passed: true, findings: expected.findings, riskFindings:expected.riskFindings.length, criteria:expected.criteria, viewIntents:expected.viewIntents.length, intentView:expected.intentView, metricCards: expected.metrics, selectedRoots: expected.scope, snapshotView: expected.snapshot, screenshots: ['summary-desktop.png','desktop.png','conformance-desktop.png','summary-mobile.png','mobile.png','conformance-mobile.png'].map(path=>join(directory,path)), externalRequests: 0, javascriptErrors: errors.length }, null, 2));
} finally {
  await send('Browser.close').catch(() => {});
  if (child.exitCode === null) child.kill('SIGTERM');
}
