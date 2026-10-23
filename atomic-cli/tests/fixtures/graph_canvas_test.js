// Injected only by the opt-in headless Chrome regression test.
setTimeout(function() {
  var results = document.createElement('pre');
  results.id = 'canvas-test-results';
  document.body.appendChild(results);
  function assert(value, message) { if (!value) throw new Error(message); }
  try {
    assert(window.d3, 'D3 must load for the canvas interaction regression');
    assert(!window.graphTestErrors.length, 'export must initialize without JavaScript errors');
    var canvas = document.getElementById('graph');
    var pane = document.getElementById('node-inspector');
    var picker = document.getElementById('node-picker');
    var dataScript = Array.from(document.scripts).find(function(script) { return script.textContent.includes('var graphData = '); });
    var graphData = JSON.parse(dataScript.textContent.split('var graphData = ')[1].split(';\n')[0]);
    var colors = new Map(graphData.nodes.map(function(node) { return [node.kind, node.color]; }));
    var legendRows = document.querySelectorAll('#legend .legend-item');
    assert(legendRows.length === colors.size, 'every visible type needs a legend entry');
    legendRows.forEach(function(row) {
      var expected = document.createElement('span');
      expected.style.backgroundColor = colors.get(row.dataset.kind);
      assert(row.querySelector('.legend-dot').style.backgroundColor === expected.style.backgroundColor, 'legend and canvas must use the same exported type color');
    });
    assert(new Set(colors.values()).size === colors.size, 'known visible types must have distinct colors');
    ['https://atomic.dev/ns/workflow#Change', 'https://atomic.dev/ns/workflow#FeatureIntent'].forEach(function(kind) {
      var usageNode = graphData.nodes.find(function(node) { return node.kind === kind; });
      assert(usageNode, 'usage fixture node must be exported');
      picker.value = usageNode.id;
      picker.dispatchEvent(new Event('change'));
      var usagePane = pane.querySelector('.inspector-ai-usage');
      assert(usagePane && usagePane.textContent.includes('$0.125000 USD'), 'change/intent must show recorded cost');
      assert(usagePane.textContent.includes('Cache read') && usagePane.textContent.includes('Recorded total'), 'token categories must be shown');
      assert(pane.textContent.includes('graph-test-model') && pane.textContent.includes('Vendor: openai') && pane.textContent.includes('Tool: CLI: graph-test-agent'), 'model, vendor and tool attribution must be visible');
    });
    document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'}));
    var view = document.getElementById('graph-view').getBoundingClientRect();
    var panel = pane.getBoundingClientRect();
    if (window.innerWidth > 760) assert(view.right <= panel.left + 1, 'right pane must not cover graph');
    else assert(view.bottom <= panel.top + 1 && panel.width > window.innerWidth - 4, 'narrow layout must stack pane below graph');
    var rect = canvas.getBoundingClientRect();
    var point = null;
    for (var y = 8; y < rect.height && !point; y += 8) {
      for (var x = 8; x < rect.width; x += 8) {
        canvas.dispatchEvent(new MouseEvent('click', {clientX: rect.left + x, clientY: rect.top + y, view: window}));
        if (pane.dataset.selectedNode) { point = [rect.left + x, rect.top + y]; break; }
      }
    }
    assert(point, 'canvas click must select a visible node');
    var first = pane.dataset.selectedNode;
    var other = Array.from(picker.options).find(function(option) { return option.value && option.value !== first; });
    assert(other, 'fixture must have multiple selectable nodes');
    function selectOther() { picker.value = other.value; picker.dispatchEvent(new Event('change')); }
    function mouse(target, type, x, y) {
      target.dispatchEvent(new MouseEvent(type, {clientX: x, clientY: y, bubbles: true, view: window, button: 0}));
    }
    selectOther();
    mouse(canvas, 'mousedown', point[0], point[1]);
    mouse(window, 'mouseup', point[0], point[1]);
    mouse(canvas, 'click', point[0], point[1]);
    assert(pane.dataset.selectedNode === first, 'real down/up/click gesture must select a node');
    assert(pane.textContent.includes('Canonical ID') && pane.textContent.includes('Metadata'), 'click shows rich details');
    selectOther();
    mouse(canvas, 'mousedown', point[0], point[1]);
    mouse(canvas, 'mousemove', point[0] + 25, point[1] + 15);
    mouse(window, 'mouseup', point[0] + 25, point[1] + 15);
    mouse(canvas, 'click', point[0] + 25, point[1] + 15);
    assert(pane.dataset.selectedNode === other.value, 'dragging must not change pinned selection');
    canvas.dispatchEvent(new WheelEvent('wheel', {clientX: point[0], clientY: point[1], deltaY: -80, bubbles: true, view: window}));
    assert(pane.dataset.selectedNode === other.value, 'zoom must preserve pinned selection');
    var neighbor = pane.querySelector('.inspector-neighbor');
    assert(neighbor, 'relationships must be navigable in the real export');
    neighbor.click();
    assert(pane.dataset.selectedNode && pane.dataset.selectedNode !== other.value, 'relationship click changes selected node');
    document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'}));
    assert(!pane.dataset.selectedNode, 'Escape clears real canvas selection');
    assert(!window.graphTestErrors.length, 'canvas and inspector interactions must not throw JavaScript errors');
    document.body.dataset.canvasTestStatus = 'passed';
    results.textContent = 'Canvas selection, dragging, zoom, relationship navigation and layout passed';
  } catch (error) {
    document.body.dataset.canvasTestStatus = 'failed';
    results.textContent = error.stack;
  }
}, 1200);
